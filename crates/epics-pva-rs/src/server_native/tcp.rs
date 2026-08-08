//! Per-connection handler.
//!
//! For each accepted client `super::accept` runs one task that:
//!
//! 1. Sends SET_BYTE_ORDER + CONNECTION_VALIDATION request
//! 2. Reads client's CONNECTION_VALIDATION response (auth)
//! 3. Sends CONNECTION_VALIDATED
//! 4. Loops reading channel ops (CREATE_CHANNEL / GET / PUT / MONITOR /
//!    GET_FIELD / DESTROY_REQUEST / DESTROY_CHANNEL).
//!
//! Channel state is kept per-connection (a `HashMap<sid, ChannelState>`).
//!
//! This module owns no socket. The connection enters through
//! `handle_connection_io`, whose reader and writer are
//! `Box<dyn AsyncRead/AsyncWrite>` trait objects — which driver produced
//! them (the host accept loop in `super::accept`, or the blocking
//! thread-per-client driver coming with RTEMS phase 6 item 7) is not
//! visible from here.
//!
//! No socket type is named anywhere in this file's production scope, and
//! `accept::tests::the_protocol_scope_owns_no_socket` keeps it that way.

// RTEMS-EXEC-MODEL-ALLOW(8): checked - these run and pass in the feature-ON suite.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::decode::{Frame, PeerRole, try_parse_frame_role};
use crate::error::{PvaError, PvaResult};
use crate::peer_buf::try_extend;
use crate::proto::{
    BitSet, ByteOrder, Command, ControlCommand, HeaderFlags, MessageType, PVA_VERSION, PvaHeader,
    QosFlags, Status, WriteExt, encode_size_into, encode_string_into,
};
use crate::pvdata::encode::{
    EncodeTypeCache, TypeCache, decode_pv_field_cached, decode_pv_field_with_bitset_cached,
    encode_pv_field, encode_type_desc, encode_type_desc_cached,
};
use crate::pvdata::{FieldDesc, NoConvert, PvField, RpcReply};

use super::config::PvaServerConfig;
use super::source::{ChannelInvalidator, DynSource, OpError};

// The accept loop moved to `super::accept` (RTEMS phase 6 item 7 stage A);
// these re-exports keep `server_native::tcp::run_tcp_server*` resolving for
// every existing caller and doc link. `accept` owns the listener socket and is
// host-only, so the re-exports carry its gate — this is the shim, not the
// protocol, and gating it is what the comment written here at the split
// already prescribed for the merge with `phase6/pva-rtems-dep-gate`.
#[cfg(not(epics_embedded_target))]
pub use super::accept::{run_tcp_server, run_tcp_server_on_listener, run_tcp_server_with_peers};

// pvxs seeds each ID namespace from a distinct non-zero base (commit
// 3b641bed) so a value used as the wrong ID type fails loudly instead of
// silently aliasing a live id of another kind. SID base = pvxs
// `serverconn.h:141` `nextSID=0x07050301`.
static NEXT_SID: AtomicU32 = AtomicU32::new(0x0705_0301);
fn alloc_sid() -> u32 {
    NEXT_SID.fetch_add(1, Ordering::Relaxed)
}

// serverChannelID sentinel in a CREATE_CHANNEL failure reply. pvxs
// uses `sid = -1` (serverchan.cpp:273/338) and wires it as 0xFFFFFFFF;
// `NEXT_SID` climbs monotonically from 0x07050301 and could never reach
// 0xFFFFFFFF in any real session, so this value can never alias a live id.
const CREATE_CHANNEL_NO_SID: u32 = u32::MAX;

/// A pvxs `ServerConn::logRemote()` diagnostic emitted during MONITOR
/// INIT option negotiation (`servermon.cpp:529,542,567,572`) when an
/// option value is PRESENT but unusable. `level` is the PVA MESSAGE
/// `messageType` byte the server sends to the client (`level2mtype`,
/// `pvaproto.h:715`): pvxs `Level::Warn` → [`MessageType::Warning`] (1),
/// `Level::Crit` → [`MessageType::Fatal`] (3). These are built ALONGSIDE
/// the effective options WITHOUT changing any negotiated value — pvxs
/// reports them while still applying its fallback (pipeline disabled,
/// default queue depth, or clamped `ackAt`).
#[derive(Debug, Clone)]
struct MonitorOptionDiag {
    level: MessageType,
    message: String,
}

/// Render a monitor `_options` value for a pvxs-shaped `logRemote` message.
///
/// pvxs streams the option `Value` through `operator<<` (`SB()<<…<<pipeline`,
/// `servermon.cpp:529`), i.e. the default TREE formatter — `<typecode> =
/// <value>` with the trailing newline the formatter always writes. That is
/// [`crate::pvdata::render_value`], the one owner shared with the QSRV
/// bridge's `record._options.process` diagnostic. This used to render a bare
/// scalar (`maybe` where pvxs sends `string = "maybe"\n`) while the bridge
/// rendered a third, differently-typed form (R10-36).
use crate::pvdata::render_value as render_option_value;

#[derive(Debug)]
struct PipelineOptions {
    enabled: bool,
    /// The NEGOTIATED per-op queue limit — pvxs `MonitorOp::limit`
    /// (`servermon.cpp:66`), seeded from
    /// [`crate::server_native::config::PvaServerConfig::monitor_queue_limit`]
    /// and overridden by a valid (`>= 2`) `record._options.queueSize`
    /// whether or not pipeline is enabled (`op->limit = qSize` sits
    /// OUTSIDE the `if(op->pipeline)` block, `:533-543`).
    ///
    /// ONE meaning on every path: it is the squash threshold, the base of
    /// the `ackAny` arithmetic, and the depth reported to the source. The
    /// port used to carry a SECOND copy (`requested_queue_size:
    /// Option<u32>`, `None` = "use the server default") whose default (64)
    /// differed from this field's (4), so a plain monitor's squash depth
    /// and its negotiated limit were different numbers (R11-31).
    ///
    /// It is NOT the initial credit window — that is the separate per-INIT
    /// `nack` rider (see [`parse_monitor_init_nack`]), which defaults to 0
    /// when absent.
    queue_size: u32,
    /// pvxs `MonitorOp::ackAt` (`servermon.cpp:68`) — the pipeline
    /// ACK-refill threshold parsed from `record._options.ackAny`. It
    /// caps the source-provided monitor watermarks at `ack_at - 1`
    /// (`servermon.cpp:332-333`, see [`clamp_watermarks`]). Defaults to
    /// `1` when `ackAny` is absent; only meaningful when `enabled`.
    ack_at: u32,
    /// pvxs `ServerConn::logRemote()` diagnostics for PRESENT-but-invalid
    /// options (`servermon.cpp:529,542,567,572`). Carried alongside the
    /// effective options so the MONITOR INIT owner can emit pvxs-shaped
    /// `CMD_MESSAGE` frames; never alters a negotiated value. Empty for a
    /// clean request.
    diagnostics: Vec<MonitorOptionDiag>,
}

/// Outcome of parsing a MONITOR INIT pvRequest's `record._options`
/// pipeline negotiation. Distinguishes the two cases the single `None`
/// return used to conflate — a parsed set of options vs. a negotiation
/// error the INIT must be rejected for.
#[derive(Debug)]
enum MonitorPipelineRequest {
    /// Parsed options to apply (pipeline on or off).
    Options(PipelineOptions),
    /// pvxs `servermon.cpp:537-540`: `pipeline=true` with a PRESENT but
    /// invalid (`<2` or unconvertible) `queueSize`. The pipeline
    /// sub-protocol requires agreement on `queueSize`, so the INIT is
    /// rejected with an error (`ctrl->error(...)` + `return`) rather
    /// than silently downgraded to a non-pipeline monitor. Carries the
    /// error text pvxs sends (`SB()<<"can not pipeline invalid queueSize : "
    /// <<queueSize`), so the offending value reaches the client — a
    /// `queueSize` the CONVERSION accepts never lands here.
    Reject(String),
}

/// pvxs `servermon.cpp:554-581` — derive the pipeline ACK-refill
/// threshold `ackAt` from `record._options.ackAny` and the negotiated
/// `queueSize` (pvxs `limit`). `ackAny` may be a plain integer (any scalar
/// [`crate::pvdata::convert::as_u32`] converts — bool, every signed/unsigned
/// integer, both reals, and a BASE-0 numeric string) or a percentage string
/// (`"N%"`). An absent or unconvertible value keeps the pvxs default of
/// `1`; an explicit `0` (below the representable minimum) clamps up to
/// `1` — CBUG-B12, where pvxs instead reads `0` as "no ackAny given" and
/// jumps it to `queueSize / 2`; the result clamps to
/// `[1, queueSize]`. `queue_size` MUST be `>= 1` (the caller only
/// invokes this for an enabled pipeline, where `queueSize >= 2`).
///
/// Returns the effective `ackAt` plus an optional pvxs
/// `Level::Crit` [`MonitorOptionDiag`]. pvxs emits a Crit `logRemote` for a
/// `"N%"` percentage string whose numeric part fails `parseTo<double>`
/// (`:561-568`), leaving `ackAt` at its default.
///
/// A plain scalar string that simply fails integer parse (e.g.
/// `"garbage"`) is SILENTLY ignored by pvxs — `as<string>` succeeds, the
/// value has no `%` suffix, so neither branch fires and no diagnostic is
/// emitted (`:560-569`). This faithfully differs from the review doc's
/// imprecise `ackAny=garbage` Crit example.
///
/// A NON-scalar `ackAny` (an array, a struct, an unselected union) is
/// [`NoConvert`] — `Err`. pvxs runs the THROWING `ackAny.as<std::string>()`
/// at `:556`, *before* the `if/else if`, and no `copyOut` arm converts those
/// storages into a string (`data.cpp:466-499`). The exception escapes
/// `handle_MONITOR` — nothing catches between there and the command-dispatch
/// `catch` in `conn.cpp:277-282`, which logs and does `bev.reset()`. pvxs
/// DROPS the circuit; it does not reply, and it does not serve the monitor.
/// (`:570-573`'s "Unable to parse …" Crit is therefore dead code: it needs
/// both conversions to fail, and any storage that fails the string one has
/// already thrown at `:556`.) The caller turns this `Err` into the port's
/// equivalent of `bev.reset()` — a fatal `PvaError` out of the TCP read loop.
fn ack_at_from(
    ack_any: Option<&PvField>,
    queue_size: u32,
) -> Result<(u32, Option<MonitorOptionDiag>), NoConvert> {
    // pvxs `MonitorOp::ackAt` struct default.
    let mut ack_at: u32 = 1;
    let mut diag: Option<MonitorOptionDiag> = None;
    if let Some(f) = ack_any {
        // `servermon.cpp:556` — `auto sval = ackAny.as<std::string>();`, the
        // THROWING form, run unconditionally ahead of both branches. Its value
        // is immediately overwritten by the `as(sval)` below, so its only
        // effect is this throw.
        let sval = crate::pvdata::convert::as_string(f)?;
        // pvxs then tries the PLAIN-INTEGER conversion FIRST — `ackAny.as(ival)`,
        // `uint32_t ival` (`:557`) — and falls to the percentage form only when
        // that conversion fails (`:560`). It runs that conversion for STRING
        // storage too: `copyOut` String → UInteger is `parseTo<uint64_t>` =
        // `stoull(s,&idx,0)`, BASE 0 (data.cpp:451-453, util.cpp:786-799). So
        // `"0x10"` is 16, `"010"` is 8, and `"-1"` wraps to `0xFFFF_FFFF` (then
        // clamps to queueSize) — all in the INTEGER branch (R9-34).
        if let Ok(n) = crate::pvdata::convert::as_u32(f) {
            ack_at = n;
        } else if let Some(pct) = sval.strip_suffix('%').filter(|p| !p.is_empty()) {
            // pvxs `else if(ackAny.as(sval))` (`:560`) — the same string the
            // throwing `as<std::string>()` above already produced. Only a `"N%"`
            // percentage does anything here.
            match pct.trim().parse::<f64>() {
                Ok(percent) => {
                    // pvxs `servermon.cpp:563` historically computed
                    // `clamp(percent,0,100) * limit` with NO `/ 100`, so any
                    // percent >= 1% saturated to the full queue after the
                    // `[1, limit]` clamp below, defeating the percentage
                    // control. Divide by 100 so `"50%"` of a queue of 4 is 2 —
                    // honoring the documented percentage semantics. pvxs adopts
                    // the same fix.
                    ack_at = (percent.clamp(0.0, 100.0) / 100.0 * queue_size as f64) as u32;
                }
                Err(e) => {
                    // pvxs `servermon.cpp:566-568`: a `"N%"` string whose
                    // numeric prefix fails `parseTo<double>` is a Crit
                    // logRemote; `ackAt` stays at its default.
                    diag = Some(MonitorOptionDiag {
                        level: MessageType::Fatal,
                        message: format!("Unable to parse% record._options.ackAny : {sval} : {e}"),
                    });
                }
            }
        }
        // else: a plain non-`%` string that is not a base-0 integer — pvxs
        // leaves `ackAt` default with NO logRemote (`:560-569` only logs the
        // `%` branch).
    }
    // servermon.cpp:581 — the requested threshold, clamped to what is
    // representable: you cannot ack at 0, and not past the queue.
    //
    // DEVIATION from C++, deliberate — CBUG-B12. pvxs runs
    // `if(op->ackAt==0u) op->ackAt = op->limit/2u;` (servermon.cpp:577-578)
    // first, reading 0 as "the caller named no threshold". It cannot mean that:
    // `MonitorOp::ackAt` is initialised to 1 (`:68`), so an ABSENT `ackAny`
    // never reaches that line — the branch fires ONLY on a value the client did
    // supply. And after `:564` (`clamp(percent,0,100)/100*limit`, truncating)
    // that is the common case: with the default limit of 4, every percentage
    // below 25% truncates to 0. The result is non-monotonic — `ackAny="25%"`
    // acks at 1 while `ackAny="10%"` acks at 2, so a client asking to ack MORE
    // eagerly gets a LAZIER threshold, and the flow-control window errs toward
    // less back-pressure, the unsafe direction for a slow client. `ackAny="0%"`
    // is not expressible at all.
    //
    // Dropping the sentinel is the whole fix: `ack_at` now means one thing (the
    // threshold the client asked for), the clamp below maps the sub-representable
    // request 0 to the minimum 1, and the percentage mapping is monotonic
    // non-decreasing. An absent `ackAny` still yields 1 — the struct default,
    // which this never touched.
    Ok((ack_at.clamp(1, queue_size), diag))
}

/// pvxs `servermon.cpp:332-333` — the pipeline ACK threshold `ack_at`
/// caps the source-provided monitor watermarks at `ack_at - 1`. The
/// single owner of this clamp: both the LOW crossing (subscriber loop)
/// and the HIGH crossing (ACK dispatch) read the level it produces, so
/// `ackAny` is honored identically on both. Levels are returned
/// unchanged for a non-pipelined monitor (`ack_at` `None`).
fn clamp_watermarks(levels: Option<(usize, usize)>, ack_at: Option<u32>) -> Option<(usize, usize)> {
    match (levels, ack_at) {
        (Some((low, high)), Some(a)) => {
            let cap = a.saturating_sub(1) as usize;
            Some((low.min(cap), high.min(cap)))
        }
        (other, _) => other,
    }
}

/// Wrap a PVA monitor event's `PvField` in the CA-side
/// [`FilteredMonitorEvent`](epics_base_rs::server::database::filters::FilteredMonitorEvent) shape so it can flow through the shared
/// channel filter framework. The CA filters operate on a Snapshot
/// (value + STAT/SEVR + time); the PVA monitor stream carries a
/// PvField tree that contains those same fields under nested
/// `value`/`alarm`/`timeStamp` members (NTScalar / NTNDArray shape).
///
/// Currently extracts:
/// * The value leaf as an `EpicsValue` — scalar OR array. Arrays are
///   carried losslessly so the `arr` transformation filter
///   sees the real array to slice. Returns `None` (fails closed) when
///   the leaf has no faithful `EpicsValue` representation — the
///   previous `Double(0.0)` fallback fabricated a stand-in value that
///   corrupted the emitted frame (see below).
/// * The mask is always set to `EventMask::VALUE` because PVA's
///   monitor stream does not carry the CA-style ALARM/PROPERTY
///   discriminator at this layer — the field bitset already encodes
///   which subfields changed.
///
/// The transformed event is bridged back to the wire by
/// [`apply_filter_transform`].
///
/// Returns `None` when the value leaf cannot be carried faithfully:
/// PVA `Boolean`, signed `Byte`, `UShort`, and `UInt` (and their
/// arrays) have no DBR-type counterpart in the `EpicsValue` model the
/// filter engine operates on. The earlier `unwrap_or(Double(0.0))`
/// turned that gap into silent corruption — a filtered
/// `NTScalar<Boolean(true)>` was emitted as `false`, and a filtered
/// `uint[]`/`ushort[]` monitor was coerced to an empty array. The
/// single owner [`apply_monitor_filter_chain`] turns `None` into a
/// monitor error rather than emit fabricated data.
fn pv_field_to_filter_event(
    value: &PvField,
) -> Option<epics_base_rs::server::database::filters::FilteredMonitorEvent> {
    use epics_base_rs::server::database::filters::FilteredMonitorEvent;
    use epics_base_rs::server::pv::MonitorEvent;
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::server::snapshot::Snapshot;
    use std::time::SystemTime;

    let val = crate::leaf_convert::pv_leaf_to_epics_value(value)?;
    Some(FilteredMonitorEvent::new(MonitorEvent {
        snapshot: std::sync::Arc::new(Snapshot::new(val, 0, 0, SystemTime::UNIX_EPOCH)),
        origin: 0,
        mask: EventMask::VALUE,
    }))
}

/// bridge a filter-chain-transformed `FilteredMonitorEvent`
/// back to the wire `PvField`. Substitutes the transformed value leaf
/// into the original monitor `PvField` (looking through an NT-style
/// `value` member) so transformation filters such as `arr` (array
/// slice) and `ts` actually change the emitted payload.
///
/// Returns `None` when the transformed value cannot be represented
/// in the original wire shape — the caller treats that as a filter
/// incompatible with the negotiated monitor descriptor.
fn apply_filter_transform(
    original: &PvField,
    transformed: &epics_base_rs::types::EpicsValue,
) -> Option<PvField> {
    let new_leaf = crate::leaf_convert::epics_value_to_pv_leaf(transformed);
    substitute_value_leaf(original, new_leaf)
}

/// Outcome of running the server-side monitor filter chain on one value.
///
/// Centralises the three-way result so the **initial-snapshot path and
/// the update loop apply the chain identically** — finding #2: the
/// initial frame previously went straight to `build_monitor_payload`,
/// so a `record._options._filter` (e.g. `arr`) sliced every update but
/// not the first frame.
enum MonitorFilterOutcome {
    /// Empty chain, or a pass/drop-only filter that passed: emit the
    /// value unchanged.
    Pass,
    /// A gating filter (`dbnd` / `dec` / `sync`) dropped this event.
    Drop,
    /// A transformation filter (`arr` / `ts`) produced a new value.
    Transformed(PvField),
    /// The filter cannot honor the negotiated monitor descriptor, for
    /// one of two reasons: the inbound value leaf has no faithful
    /// `EpicsValue` representation for the filter engine (forward
    /// fail-closed — never fabricate a stand-in), or the filter
    /// rewrote the leaf to a type that does not match the descriptor's
    /// value-leaf type (backward fail-closed — never coerce a wrong
    /// type onto the wire). Either way the subscription emits a monitor
    /// error rather than a corrupted frame.
    DescriptorMismatch,
}

/// Run the server-side channel-filter chain on one monitor value. The
/// single owner of "apply the filter to an outbound frame", shared by
/// every emission point so a frame is never emitted unfiltered while a
/// sibling frame is filtered. An empty chain is a no-op pass-through.
fn apply_monitor_filter_chain(
    filters: &epics_base_rs::server::database::filters::FilterChain,
    value: &PvField,
    intro: &FieldDesc,
) -> MonitorFilterOutcome {
    if filters.is_empty() {
        return MonitorFilterOutcome::Pass;
    }
    // Forward bridge: a value leaf the DBR filter engine cannot carry
    // faithfully fails closed here — no `Double(0.0)` stand-in.
    let Some(fev) = pv_field_to_filter_event(value) else {
        return MonitorFilterOutcome::DescriptorMismatch;
    };
    match filters.apply(fev) {
        None => MonitorFilterOutcome::Drop,
        Some(transformed) => {
            // Backward bridge: a filter may rewrite the value leaf to a
            // different type (e.g. `ts` replaces the value with a
            // timestamp `Int64`/`Double`/`String`). The substituted
            // leaf MUST match the negotiated descriptor's value-leaf
            // type EXACTLY — scalar type, or array element type. A
            // struct_id-only check let a type-changed NT `value` member
            // reach the encoder's coercing fallback.
            match apply_filter_transform(value, &transformed.event.snapshot.value) {
                Some(tv) if transformed_leaf_fits_descriptor(&tv, intro) => {
                    MonitorFilterOutcome::Transformed(tv)
                }
                _ => MonitorFilterOutcome::DescriptorMismatch,
            }
        }
    }
}

/// Replace the value leaf of `original` with `new_leaf`. For an
/// NT-style structure the `value` member is replaced in place;
/// for a bare scalar/array the whole field is replaced.
fn substitute_value_leaf(original: &PvField, new_leaf: PvField) -> Option<PvField> {
    match original {
        PvField::Scalar(_) | PvField::ScalarArray(_) | PvField::ScalarArrayTyped(_) => {
            Some(new_leaf)
        }
        PvField::Structure(s) => {
            let mut out = s.clone();
            let slot = out.fields.iter_mut().find(|(k, _)| k == "value")?;
            slot.1 = new_leaf;
            Some(PvField::Structure(out))
        }
        _ => None,
    }
}

/// The value leaf of a monitor value, looking through an NT-style
/// structure's `value` member. Mirrors [`substitute_value_leaf`]'s
/// notion of "the leaf a transformation filter replaces".
fn value_leaf_of(field: &PvField) -> Option<&PvField> {
    match field {
        PvField::Scalar(_) | PvField::ScalarArray(_) | PvField::ScalarArrayTyped(_) => Some(field),
        PvField::Structure(s) => s
            .fields
            .iter()
            .find_map(|(k, v)| (k == "value").then_some(v)),
        _ => None,
    }
}

/// The value-leaf descriptor of a monitor descriptor, looking through
/// an NT-style structure's `value` member — the descriptor-side mirror
/// of [`value_leaf_of`].
fn value_leaf_desc_of(desc: &FieldDesc) -> Option<&FieldDesc> {
    match desc {
        FieldDesc::Scalar(_) | FieldDesc::ScalarArray(_) => Some(desc),
        FieldDesc::Structure { fields, .. } => {
            fields.iter().find_map(|(k, d)| (k == "value").then_some(d))
        }
        _ => None,
    }
}

/// True iff a filter-transformed value's leaf fits the negotiated
/// monitor descriptor's value-leaf type EXACTLY (scalar type, or array
/// element type). This is the gate that keeps a filter from emitting a
/// frame whose value-leaf type differs from the descriptor the channel
/// was opened with.
///
/// [`crate::pvdata::value_matches_descriptor`] compares only
/// `struct_id` for an NT structure and only the array *shape* for an
/// array, so a `ts` filter that rewrites an `NTScalar<Double>` value to
/// an `Int64` timestamp passed that looser check and reached the
/// encoder's coercing fallback. An empty array carries no element type
/// and cannot corrupt data, so it always fits.
fn transformed_leaf_fits_descriptor(value: &PvField, desc: &FieldDesc) -> bool {
    let (Some(leaf), Some(leaf_desc)) = (value_leaf_of(value), value_leaf_desc_of(desc)) else {
        return false;
    };
    match (leaf, leaf_desc) {
        (PvField::Scalar(sv), FieldDesc::Scalar(st)) => sv.scalar_type() == *st,
        (PvField::ScalarArray(items), FieldDesc::ScalarArray(st)) => {
            items.iter().all(|it| it.scalar_type() == *st)
        }
        (PvField::ScalarArrayTyped(t), FieldDesc::ScalarArray(st)) => t.scalar_type() == *st,
        _ => false,
    }
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
            PvField::Scalar(ScalarValue::String(s)) => Some(s.as_str_lossy().into_owned()),
            _ => None,
        })
    })?;
    if json.trim().is_empty() {
        None
    } else {
        Some(json)
    }
}

/// Consume the optional u32 `nack` (initial pipeline window) that a
/// pvxs client appends to a MONITOR INIT body when it sets the
/// pipeline bit (pvxs `servermon.cpp:494-495` / `clientmon.cpp:341-342`).
///
/// - kind mismatch or pipeline bit clear → `Ok(None)` (no nack to read).
/// - bit set AND four bytes present → `Ok(Some(nack))`.
/// - bit set but the four bytes are truncated → `Err` (FATAL). pvxs
///   reads the nack unconditionally once the bit is set and resets the
///   connection on `!M.good()` (`servermon.cpp:494-503`), so a truncated
///   nack is a framing violation, not a legacy omission.
///
/// A missing rider (`Ok(None)`) seeds the credit window to 0 at the call
/// site, matching pvxs `nack = 0` default + `op->window = nack`. pvxs
/// logs "pipeline monitor w/o initial nack incompatible" whenever the
/// negotiated window is 0 (`op->pipeline && !nack`, `servermon.cpp:546-552`)
/// — that covers both an absent rider and a present `nack == 0`, since
/// both leave `nack == 0`; the caller emits the same warning.
fn parse_monitor_init_nack(
    kind: OpKind,
    subcmd: u8,
    cur: &mut std::io::Cursor<&[u8]>,
    order: ByteOrder,
) -> Result<Option<u32>, PvaError> {
    if kind != OpKind::Monitor || (subcmd & 0x80) == 0 {
        return Ok(None);
    }
    cur.get_u32(order).map(Some).map_err(|e| {
        PvaError::Decode(format!(
            "malformed MONITOR INIT: pipeline bit set but initial nack u32 truncated: {e}"
        ))
    })
}

/// Inspect a decoded pvRequest for `record._options.pipeline`,
/// `record._options.queueSize` and `record._options.ackAny` — pvxs
/// `ServerConn::handle_MONITOR`'s INIT half (`servermon.cpp:519-582`).
///
/// `default_limit` is the limit a fresh `MonitorOp` starts with (pvxs
/// `limit=4u`, `servermon.cpp:66`; here
/// [`crate::server_native::config::PvaServerConfig::monitor_queue_limit`]).
/// Every returned [`PipelineOptions`] names a resolved
/// [`PipelineOptions::queue_size`], so no caller re-derives the depth.
///
/// [`MonitorPipelineRequest::Options`] on success — including for a
/// pvRequest with no `record._options` at all, which negotiates nothing and
/// therefore lands on the defaults. [`MonitorPipelineRequest::Reject`] when
/// `pipeline=true` is paired with a PRESENT-but-invalid `queueSize` (pvxs
/// `servermon.cpp:537-540`).
///
/// `Err(NoConvert)` is pvxs's THIRD outcome, and the only one that is not a
/// reply: an `ackAny` whose storage no `copyOut` arm converts throws out of
/// `handle_MONITOR` and resets the circuit (see [`ack_at_from`]). The caller
/// must fail the connection, not answer the INIT.
fn monitor_pipeline_options(
    req: &PvField,
    default_limit: u32,
) -> Result<MonitorPipelineRequest, NoConvert> {
    let plain = || {
        MonitorPipelineRequest::Options(PipelineOptions {
            enabled: false,
            queue_size: default_limit,
            ack_at: 1,
            diagnostics: Vec::new(),
        })
    };
    let PvField::Structure(root) = req else {
        return Ok(plain());
    };
    let Some(PvField::Structure(record_s)) = root
        .fields
        .iter()
        .find_map(|(k, v)| (k == "record").then_some(v))
    else {
        return Ok(plain());
    };
    let Some(PvField::Structure(opt_s)) = record_s
        .fields
        .iter()
        .find_map(|(k, v)| (k == "_options").then_some(v))
    else {
        return Ok(plain());
    };
    // pvxs `ServerConn::logRemote()` diagnostics for PRESENT-but-invalid
    // options, accumulated alongside the effective options without
    // altering any negotiated value.
    let mut diagnostics: Vec<MonitorOptionDiag> = Vec::new();
    // pvxs `servermon.cpp:523-531` — `pipeline.as(v)` with `bool v`, i.e.
    // `Value::as(bool&)`: ONE conversion ([`crate::pvdata::convert::as_bool`],
    // the port's `copyOut`/`copyOutScalar` owner), not a type test. Bool,
    // every signed/unsigned integer, AND both reals convert by C's
    // `bool(src)` non-zero rule (`data.cpp:405`); a string converts only as
    // the exact tokens `"true"`/`"false"` (`data.cpp:466-469`). Anything the
    // conversion refuses — an unrecognized string, an array, a struct — is
    // `NoConvert` → `as(v)` answers false → pvxs leaves `op->pipeline` at its
    // `false` default and emits a `Level::Warn` logRemote (`:529`).
    //
    // The hand-rolled per-variant match this replaces diverged twice: it
    // hardcoded Float/Double to `false` (a pvxs client sending
    // `pipeline = Double(1.0)` ran the credit-windowed pipeline sub-protocol
    // against pvxs and a plain monitor here — R10-31), and it accepted
    // `"1"`/`"yes"`/`"0"`/`"no"` strings that pvxs's `as<bool>` refuses. The
    // QSRV side already had this conversion right for
    // `record._options.atomic`/`block`/`process`; both now share the one owner.
    let pipeline_field = opt_s
        .fields
        .iter()
        .find_map(|(k, v)| (k == "pipeline").then_some(v));
    let enabled = match pipeline_field {
        None => false,
        Some(f) => match crate::pvdata::convert::as_bool(f) {
            Ok(v) => v,
            Err(_) => {
                diagnostics.push(MonitorOptionDiag {
                    level: MessageType::Warning,
                    message: format!(
                        "Unable to parse record._options.pipeline : {}",
                        render_option_value(f)
                    ),
                });
                false
            }
        },
    };
    // pvxs `servermon.cpp:533-540` distinguishes a PRESENT-but-invalid
    // `queueSize` from an ABSENT one: `if(auto queueSize = ...)` gates
    // on presence, then `queueSize.as(qSize) && qSize>=2` gates on
    // validity. Track both — the `queue_size` parse collapses
    // "absent" and "present-unparseable" to `None`, so presence must be
    // observed separately.
    let queue_size_field = opt_s
        .fields
        .iter()
        .find_map(|(k, v)| (k == "queueSize").then_some(v));
    let queue_size_present = queue_size_field.is_some();
    // pvxs `queueSize.as(qSize)` with `uint32_t qSize` (`servermon.cpp:534`) —
    // the same one conversion `pipeline` uses, targeting `StoreType::UInteger`
    // ([`crate::pvdata::convert::as_u32`]). It converts a real
    // (`uint64_t(double(src))`) and parses a string with `parseTo<uint64_t>` =
    // `stoull(s, &idx, 0)`, i.e. BASE 0 — so `Double(8.0)` is 8, `"0x10"` is
    // 16, `"010"` is 8. The hand-rolled match this replaces dropped
    // Float/Double entirely and parsed strings as decimal-only, so a
    // `queueSize` pvxs converts was rejected here: an enabled pipeline got the
    // port's invented "can not pipeline invalid queueSize" error and a plain
    // monitor got a spurious Warn plus the default depth (R10-32). A negative
    // or oversized integer WRAPS through the uint64 cast and the uint32
    // narrowing rather than being refused (`Int(-1)` → `0xFFFF_FFFF`), which is
    // a valid — enormous — limit; the monitor queue grows lazily, so a hostile
    // value costs no memory until the events actually arrive.
    let queue_size = queue_size_field.and_then(|v| crate::pvdata::convert::as_u32(v).ok());
    // pvxs `servermon.cpp:554` — `record._options.ackAny`. Parsed only
    // for an enabled pipeline (`ackAt` is meaningless without one).
    let ack_any = opt_s
        .fields
        .iter()
        .find_map(|(k, v)| (k == "ackAny").then_some(v));
    // pvxs `servermon.cpp:533-543`: `uint32_t qSize = op->limit;` then
    // `if(queueSize.as(qSize) && qSize>=2) op->limit = qSize;`. ONE limit,
    // seeded with the per-op default and overridden by a valid request —
    // the assignment sits OUTSIDE `if(op->pipeline)`, so it holds for a
    // plain monitor too. Everything downstream (squash threshold, `ackAt`
    // arithmetic, reported depth) reads this single value.
    let requested_limit = queue_size.filter(|&n| n >= 2);
    let limit = requested_limit.unwrap_or(default_limit);
    // present + not-accepted == pvxs's `!(queueSize.as(qSize) && qSize>=2)`
    // (an unconvertible value or a `< 2` one). A CONFIGURED default that
    // happens to equal the rejected request does not make it accepted.
    let queue_size_invalid = queue_size_present && requested_limit.is_none();
    if enabled && queue_size_invalid {
        // PRESENT but invalid (`<2` or unconvertible) under pipeline: pvxs
        // `servermon.cpp:537-540` rejects the INIT — the pipeline
        // sub-protocol requires agreement on `queueSize`. Do NOT downgrade
        // to a non-pipeline monitor. pvxs answers this with
        // `ctrl->error(...)` (a per-op error), NOT a logRemote, and returns
        // BEFORE the `ackAny` block — so no diagnostics are owed.
        // `diagnostics` is provably empty here (a rejected pipeline
        // required a recognized `pipeline=true`, which emits no Warn, and
        // `ackAny` is never reached), so dropping it matches pvxs.
        let rendered = queue_size_field
            .map(render_option_value)
            .unwrap_or_default();
        return Ok(MonitorPipelineRequest::Reject(format!(
            "can not pipeline invalid queueSize : {rendered}"
        )));
    }
    if queue_size_invalid {
        // pvxs `servermon.cpp:541-543`: the same present-but-invalid
        // `queueSize` on a NON-pipeline monitor keeps the default depth and
        // emits a Warn logRemote ("Unable to use …").
        let rendered = queue_size_field
            .map(render_option_value)
            .unwrap_or_default();
        diagnostics.push(MonitorOptionDiag {
            level: MessageType::Warning,
            message: format!("Unable to use record._options.queueSize : {rendered}"),
        });
    }
    // pvxs parses `ackAny` only inside `if(op->pipeline)` (`:546-582`);
    // without a credit window there is no ACK cadence to threshold, so a
    // plain monitor keeps `ackAt` at its `1` initializer (`:68`).
    let ack_at = if enabled {
        let (ack_at, ack_diag) = ack_at_from(ack_any, limit)?;
        diagnostics.extend(ack_diag);
        ack_at
    } else {
        1
    };
    Ok(MonitorPipelineRequest::Options(PipelineOptions {
        enabled,
        queue_size: limit,
        ack_at,
        diagnostics,
    }))
}

#[derive(Clone)]
#[allow(dead_code)]
struct ChannelState {
    name: String,
    cid: u32,
    sid: u32,
    /// Channel-invariant negotiated descriptor, shared by refcount with
    /// every op minted on this channel — a per-op deep clone of a full
    /// NTScalar tree was 11% of server CPU under a PUT load.
    introspection: Option<Arc<FieldDesc>>,
    /// Source bound at CREATE_CHANNEL that owns this channel. Every
    /// operation (GET/PUT/MONITOR/RPC/PROCESS/GET_FIELD) dispatches
    /// through this owner instead of re-resolving the top-level source
    /// registry per operation, so a live channel cannot silently change
    /// owner when a source is added or removed. pvxs binds the accepting
    /// source's callbacks into the `ServerChan` at CREATE_CHANNEL
    /// (`serverchan.cpp:70-112`); a later `removeSource` does not rewrite
    /// them (`server.cpp:100-112`).
    source: DynSource,
    /// Shared per-channel report counters (name + tx/rx + ReportInfo),
    /// the SAME `Arc` registered in this connection's `PeerEntry` under
    /// the channel's SID. Handlers attribute per-PV traffic through this
    /// (`stat.add_tx`/`add_rx`) so `PvaServer::report`
    /// can show per-channel byte counters (pvxs `chan->statTx/statRx`,
    /// server.cpp:260-268).
    stat: Arc<crate::server_native::peers::ChannelStat>,
    /// Credential snapshot taken when this channel was CREATED, used for
    /// the channel *lifecycle* callbacks (`notify_channel_open` /
    /// `notify_channel_close`) and nothing else. pvxs builds the channel's
    /// `ServerChannelControl` with `conn->cred` at CREATE_CHANNEL
    /// (`serverchan.cpp:62`); a later re-auth that reassigns `ServerConn::cred`
    /// does NOT rewrite the credential captured by an already-open channel
    /// control. Per-operation handlers still use the connection's *current*
    /// credential (pvxs builds each `ConnectOp`/`ExecOp` from `conn->cred`),
    /// so only the open/close edges are pinned here.
    open_cred: ClientCredentials,
    /// ioid → (introspection negotiated for this op, kind)
    ops: HashMap<u32, OpState>,
}

// `source` is a `dyn ChannelSourceObj` trait object with no `Debug`
// bound, so `ChannelState` cannot derive `Debug`; print the bound
// owner as an opaque marker instead.
impl std::fmt::Debug for ChannelState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelState")
            .field("name", &self.name)
            .field("cid", &self.cid)
            .field("sid", &self.sid)
            .field("introspection", &self.introspection)
            .field("source", &"<bound owner>")
            .field("stat", &self.stat)
            .field("open_cred", &self.open_cred)
            .field("ops", &self.ops)
            .finish()
    }
}

/// Shared abort guard: when the last clone is dropped (HashMap removal,
/// connection end, ...), the spawned task is aborted automatically.
#[derive(Debug)]
struct AbortOnDrop(epics_base_rs::runtime::task::TaskAbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Run a data-phase EXEC body inline on the read-loop thread when it can
/// complete without waiting; promote it to a spawned task only when it
/// cannot.
///
/// pvxs executes GET/PUT/RPC callbacks synchronously on the connection's
/// event-loop thread and replies from the same stack. Spawning a task per
/// EXEC instead costs several cross-thread futex wakes per operation
/// (worker wake, reply-channel wake, `ExecFinished` wake back to the read
/// loop) — measured ~8.8 wakes per wire PUT against pvxs's ~0.1. One poll
/// under a noop waker lets the common already-ready body (local source,
/// uncontended locks, writer channel below capacity) finish with no task
/// at all; a body that genuinely waits (remote-fronting source, writer
/// backpressure) is handed to `spawn`, which schedules an immediate first
/// poll that re-registers every waker the noop poll discarded.
///
/// Returns the abort handle when promoted, `None` when the body already
/// ran to completion (nothing left to abort). Either way the body's
/// [`ExecFinishGuard`] has queued (or will queue) its `ExecFinished`, and
/// the read loop drains that queue only after the caller's
/// [`finish_exec_data_task`] bookkeeping ran, so the `last_request`
/// disposition in [`apply_exec_finish`] is unchanged.
fn poll_inline_or_spawn<F>(fut: F) -> Option<epics_base_rs::runtime::task::TaskAbortHandle>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let mut fut = Box::pin(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match std::future::Future::poll(fut.as_mut(), &mut cx) {
        std::task::Poll::Ready(()) => None,
        std::task::Poll::Pending => Some(epics_base_rs::runtime::task::spawn(fut).abort_handle()),
    }
}

/// Apply the data-phase op continuation after a GET/PUT/RPC EXEC body
/// has been started by [`poll_inline_or_spawn`].
///
/// pvxs records `op->lastRequest = subcmd & 0x10` *while the op stays
/// `Executing` in `opByIOID`* (`serverget.cpp:470-471`) and only
/// `cleanup()`s it — freeing the IOID — *after* the reply has been
/// serialized (`serverget.cpp:111-114`, inside `doReply`). Freeing the IOID
/// at spawn time (the pre-fix behaviour) opened a reuse race: a client could
/// send a last-request EXEC, immediately re-INIT the same IOID before the
/// slow source replied, and have the still-in-flight first reply and the new
/// operation collide on one IOID on the wire.
///
/// The Rust EXEC handlers serialize their response from a body that
/// cannot reach `ch.ops`, so the read-loop owner defers cleanup to
/// [`apply_exec_finish`], which fires when the body's [`ExecFinishGuard`]
/// signals completion (right after the reply is handed to the writer). Both
/// branches therefore keep the op reserved and install the abort guard; only
/// the bookkeeping differs:
///
/// - last request (`subcmd & 0x10`): mark `last_request` so the completion
///   owner removes the op (matching pvxs `ServerOp::cleanup`) once the reply
///   is out, not before.
/// - otherwise: leave it `Executing`; the completion owner returns it to
///   `Idle` so a later explicit re-EXEC is accepted.
///
/// `abort` is `None` when the body completed inline — there is no task to
/// abort, and the op's guard slot is already `None` (an `Idle` op holds no
/// guard, and `begin_exec` only admits `Idle` ops).
fn finish_exec_data_task(
    ch: &mut ChannelState,
    ioid: u32,
    subcmd: u8,
    abort: Option<epics_base_rs::runtime::task::TaskAbortHandle>,
) {
    if let Some(op_mut) = ch.ops.get_mut(&ioid) {
        if subcmd & QosFlags::DESTROY != 0 {
            op_mut.last_request = true;
        }
        op_mut.data_task_abort = abort.map(|a| Arc::new(AbortOnDrop(a)));
    }
}

/// terminal signal a spawned MONITOR subscriber task sends to
/// its read-loop owner when the task ends.
///
/// The subscriber task owns no handle to `channels`, so it cannot remove
/// its own op from `ch.ops` when the monitor ends (source close,
/// descriptor change, ACL denial, filter/transform mismatch, raw
/// re-encode terminal). It instead reports its identity here and the
/// read-loop owner — the single actor that may mutate `channels` — runs
/// the same removal `DESTROY_REQUEST` runs. pvxs models this as
/// `servermon.cpp:148-150` calling `ServerOp::cleanup()`
/// (`serverconn.cpp:487-508`) before emitting the FINISH reply.
///
/// `op_id` is the op instance's process-unique [`OpState::monitor_op_id`].
/// It tags THIS subscriber instance so a signal that arrives late — an
/// aborted task whose future is dropped only after its ioid was already
/// removed and re-INIT'd — cannot evict the *fresh* op that reused the
/// ioid. The owner removes only when the live op's id still matches
/// ([`apply_monitor_finish`]).
#[derive(Debug, Clone, Copy)]
struct MonitorFinished {
    sid: u32,
    ioid: u32,
    op_id: u64,
}

/// RAII finalizer held as a single local at the top of the
/// MONITOR subscriber task body. Its `Drop` reports completion to the
/// read-loop owner on EVERY task exit — the normal source-close FINISH,
/// an early `return` for an ACL-deny / descriptor-change / filter-
/// mismatch / raw re-encode terminal, a panic, or an `AbortOnDrop`-driven
/// cancellation (DESTROY / channel teardown / disconnect). Because it is
/// one local that no path can step around, the op-removal invariant holds
/// by construction rather than by remembering to signal on each exit
/// (see CLAUDE.md "Strong state transitions").
struct MonitorFinishGuard {
    tx: mpsc::UnboundedSender<MonitorFinished>,
    fin: MonitorFinished,
}

impl Drop for MonitorFinishGuard {
    fn drop(&mut self) {
        // Unbounded so this sync Drop never loses the signal to a full
        // channel; the only `send` failure is a closed receiver, which
        // means the read loop already ended and dropped every op.
        let _ = self.tx.send(self.fin);
    }
}

/// read-loop owner side of a MONITOR subscriber's terminal
/// signal. Mirrors [`handle_destroy_request`]'s removal — dropping the
/// `OpState` drops `monitor_start_ctl` (terminal
/// `notify_monitor_start(false)`) and `monitor_abort` (subscriber abort,
/// already a no-op for a task that ended on its own) — but gated on the
/// op-instance id so a stale signal cannot evict a re-INIT'd op that
/// reused the ioid (the ABA guard described on [`MonitorFinished`]).
fn apply_monitor_finish(channels: &mut HashMap<u32, ChannelState>, fin: MonitorFinished) {
    if let Some(ch) = channels.get_mut(&fin.sid)
        && ch.ops.get(&fin.ioid).map(|op| op.monitor_op_id) == Some(fin.op_id)
    {
        ch.ops.remove(&fin.ioid);
    }
}

/// in-flight state of a non-monitor (GET/PUT/RPC/PUT_GET/PROCESS)
/// data-phase op. pvxs models this as `ServerOp::state`
/// (`serverget.cpp:467-476`, `:511-514`): a data-phase EXEC runs only when
/// the op is `Idle`, flips it to `Executing`, and the op returns to `Idle`
/// only when the original callback replies (`:112-116`). A second EXEC while
/// `Executing` is logged and IGNORED — the first task is NOT cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecState {
    /// No data-phase task in flight; an EXEC is accepted here.
    Idle,
    /// A spawned data-phase task is running; a further EXEC is ignored.
    Executing,
}

/// begin a data-phase EXEC. Returns `Some(op_id)` when the op was
/// `Idle` (now transitioned to `Executing`) and the caller may spawn the
/// task, or `None` to IGNORE this EXEC — either the op is already
/// `Executing` (pvxs `serverget.cpp:511-514`: log + drop, do NOT abort the
/// in-flight task) or the op no longer exists. The returned `op_id`
/// (`OpState::monitor_op_id`, minted per op at INIT) tags this exec instance
/// for the [`ExecFinished`] ABA guard.
fn begin_exec(ch: &mut ChannelState, ioid: u32) -> Option<u64> {
    match ch.ops.get_mut(&ioid) {
        Some(op) if op.exec_state == ExecState::Executing => None,
        Some(op) => {
            op.exec_state = ExecState::Executing;
            Some(op.monitor_op_id)
        }
        None => None,
    }
}

/// terminal signal a spawned data-phase task sends to its read-loop
/// owner when the task ends (mirrors [`MonitorFinished`] for the
/// non-monitor lifecycle). The spawned task owns no handle to `channels`, so
/// it cannot return its own op to `Idle` when its response is sent; it
/// reports its identity here and the read-loop owner — the single actor that
/// may mutate `channels` — applies the transition. pvxs returns the op to
/// `Idle` (or cleans it up on `lastRequest`) when the callback replies
/// (`serverget.cpp:112-116`).
///
/// `op_id` is the op-instance's process-unique [`OpState::monitor_op_id`]. It
/// tags THIS exec instance so a signal that arrives late (an aborted task
/// whose future drops after its ioid was removed and re-INIT'd) cannot flip a
/// *fresh* op back to `Idle` ([`apply_exec_finish`] ABA guard).
#[derive(Debug, Clone, Copy)]
struct ExecFinished {
    sid: u32,
    ioid: u32,
    op_id: u64,
    /// Whether the data-phase task emitted a SUCCESSFUL terminal reply
    /// (`Status::ok`), as opposed to an error reply / panic / abort.
    ///
    /// For a GPR op (GET/PUT/RPC) the read-loop owner cleans up a
    /// `last_request` op ONLY after success; an error reply returns the op to
    /// `Idle` with the marker preserved for a later EXEC (pvxs
    /// `ServerGPR::doReply`, `serverget.cpp:86-116`). For one-shot /
    /// pvAccessCPP-modeled kinds (GET_FIELD, PUT_GET, PROCESS, ARRAY) the owner
    /// ignores this flag and removes a `last_request` op on every terminal
    /// reply (see [`apply_exec_finish`]); those tasks leave it `false`.
    ///
    /// Defaulting to `false` makes the safe disposition (return to Idle for a
    /// GPR op, never an erroneous destroy) hold by construction for any task
    /// exit that does not explicitly [`ExecFinishGuard::mark_success`] — error
    /// returns, panics, and aborts all skip the marking.
    success: bool,
}

/// RAII finalizer held as a single local at the top of a data-phase
/// task body. Its `Drop` reports completion to the read-loop owner on EVERY
/// task exit — the normal reply-then-return, an early `return` on an error
/// reply, a panic, or an `AbortOnDrop`-driven cancellation (DESTROY / channel
/// teardown / disconnect). Because it is one local that no path can step
/// around, the "executing op returns to idle when its task ends" invariant
/// holds by construction (see CLAUDE.md "Strong state transitions").
struct ExecFinishGuard {
    tx: mpsc::UnboundedSender<ExecFinished>,
    fin: ExecFinished,
}

impl ExecFinishGuard {
    /// Record that this task emitted a successful terminal reply
    /// (`Status::ok`). The `Drop` finalizer then tells the owner to clean up a
    /// `last_request` GPR op; without this call (error reply, panic, abort) the
    /// owner returns the op to `Idle` with the marker preserved, mirroring pvxs
    /// `ServerGPR::doReply` (`serverget.cpp:86-116`).
    fn mark_success(&mut self) {
        self.fin.success = true;
    }
}

impl Drop for ExecFinishGuard {
    fn drop(&mut self) {
        // Unbounded so this sync Drop never loses the signal to a full
        // channel; the only `send` failure is a closed receiver (the read
        // loop already ended and dropped every op).
        let _ = self.tx.send(self.fin);
    }
}

/// read-loop owner side of a data-phase task's terminal signal.
/// Gated on the op-instance id so a stale signal cannot affect a re-INIT'd op
/// that reused the ioid.
///
/// Disposition depends on the op kind, because pvxs models GPR and one-shot
/// ops with different `doReply` lifecycles:
///
/// - GPR (`Get`/`Put`/`Rpc`, pvxs `ServerGPR::doReply`,
///   `serverget.cpp:86-116`): a SUCCESSFUL reply on a `last_request` op cleans
///   it up (`:112-114`); a SUCCESSFUL non-last reply, OR ANY error reply,
///   returns the op to `Idle` (`:89-90`, `:114-115`) WITHOUT touching
///   `last_request` — the sticky marker survives an error for a later EXEC.
///   So removal requires `last_request && success`; otherwise return to Idle.
/// - one-shot / pvAccessCPP-modeled (`GetField`/`PutGet`/`Process`/`Array`):
///   `GET_FIELD` is a `ServerIntrospect` removed on EVERY terminal reply,
///   success or error (`serverintrospect.cpp:47-49`; the Rust slow path always
///   reserves it as `last_request`); `PUT_GET`/`PROCESS`/`ARRAY` follow the
///   pvAccessCPP destroy-after-reply lifecycle. These keep the unconditional
///   `last_request` rule and ignore `success`.
///
/// In every case the reply has already been sent, so this is where the IOID —
/// kept reserved until exactly this point so a re-INIT racing a slow source
/// could not reuse it mid-reply — is finally freed (on removal) and the
/// (now-inert) abort guard is cleared (on return-to-Idle).
fn apply_exec_finish(channels: &mut HashMap<u32, ChannelState>, fin: ExecFinished) {
    let Some(ch) = channels.get_mut(&fin.sid) else {
        return;
    };
    let (matches, last_request, kind) = match ch.ops.get(&fin.ioid) {
        Some(op) => (op.monitor_op_id == fin.op_id, op.last_request, op.kind),
        None => return,
    };
    if !matches {
        return;
    }
    let remove = match kind {
        // GPR cleanup is gated on a
        // successful reply; an error reply keeps the op Idle with the sticky
        // last_request marker for a later EXEC.
        OpKind::Get | OpKind::Put | OpKind::Rpc => last_request && fin.success,
        // GET_FIELD one-shot + pvAccessCPP PUT_GET/PROCESS/ARRAY: removed once
        // the single reserved reply is out, regardless of status.
        _ => last_request,
    };
    if remove {
        ch.ops.remove(&fin.ioid);
    } else if let Some(op) = ch.ops.get_mut(&fin.ioid) {
        op.exec_state = ExecState::Idle;
        op.data_task_abort = None;
    }
}

/// True when `ioid` already names a live operation on *any* channel of
/// this connection.
///
/// pvxs scopes operation IDs to the whole connection, not to one channel:
/// `ServerConn::opByIOID` (`serverconn.h:142`) is the connection-wide map an
/// INIT consults to reject a reused IOID (`serverget.cpp:378-384`,
/// `servermon.cpp:505-511`, `serverintrospect.cpp:157-178`). Modelling that as
/// the per-channel `ChannelState::ops` lets two channels hold the same IOID,
/// and because operation replies are tagged by IOID alone the two reply streams
/// become indistinguishable to the client. The single source of truth stays
/// `channels`; this helper widens the uniqueness *scope* to the connection so
/// the duplicate-IOID rule holds across channels by construction rather than
/// maintaining a redundant secondary index that could desync.
fn ioid_live_on_conn(channels: &HashMap<u32, ChannelState>, ioid: u32) -> bool {
    channels.values().any(|c| c.ops.contains_key(&ioid))
}

/// SID of the channel that owns the operation `ioid`, scanning the whole
/// connection. pvxs keys CANCEL/DESTROY/MESSAGE on the connection-wide
/// `opByIOID` and only then consults the SID (`serverconn.cpp:262-346`); with
/// connection-wide IOID uniqueness an IOID maps to at most one channel.
fn op_owner_sid(channels: &HashMap<u32, ChannelState>, ioid: u32) -> Option<u32> {
    channels
        .iter()
        .find_map(|(sid, c)| c.ops.contains_key(&ioid).then_some(*sid))
}

/// Resolve which channel should service a data-phase (non-INIT) operation
/// frame for `ioid`. pvxs looks the operation up in the connection-wide
/// `opByIOID` map and acts on `op->chan`, IGNORING the SID carried in the
/// frame for GET/PUT/RPC EXEC (`serverget.cpp:421-423`/`461-465`). A
/// MONITOR frame additionally requires the supplied SID to equal the
/// owning channel's SID and resets the circuit on a mismatch
/// (`servermon.cpp:610-635`).
///
/// - `Ok(Some(owner))` — the IOID is live; service the frame on `owner`'s
///   channel regardless of the SID in the frame.
/// - `Ok(None)` — the IOID is not live anywhere on the connection; the
///   caller falls back to its existing not-found path (the DESTROY race,
///   `serverget.cpp:423-428` / `servermon.cpp:611-617`).
/// - `Err` — connection-fatal: a MONITOR frame named a SID that does not
///   own the IOID.
///
/// Keying the data phase on the op owner (not the frame SID) closes the
/// gap where a data frame whose IOID is live on another channel was
/// silently dropped because the frame SID's channel did not hold the op.
fn data_phase_owner_sid(
    channels: &HashMap<u32, ChannelState>,
    ioid: u32,
    frame_sid: u32,
    require_sid_match: bool,
) -> Result<Option<u32>, PvaError> {
    match op_owner_sid(channels, ioid) {
        Some(owner) if require_sid_match && owner != frame_sid => Err(PvaError::Decode(format!(
            "MONITOR data-phase SID {frame_sid} does not own IOID {ioid} \
             (owner channel {owner}); pvxs servermon.cpp:610-635 protocol error"
        ))),
        other => Ok(other),
    }
}

/// await a user-supplied source handler so that a panic inside it
/// becomes a recoverable `Err` instead of unwinding the spawned exec task.
///
/// Each data-phase exec task builds and sends its single client reply *after*
/// awaiting the handler (GET/PUT/PUT_GET/PROCESS/RPC). An uncaught panic would
/// skip that reply: [`ExecFinishGuard`] still returns the op to `Idle`, but the
/// client receives no response and waits out its full operation timeout. By
/// mapping the panic to `Err`, the caller routes it into the same
/// `Status::error` reply path a returned `Err` already uses, so every exec exit
/// emits exactly one reply. The abort/cancel path (DESTROY / channel teardown)
/// drops the future before this returns and so still sends nothing, which is
/// the correct behavior for a destroyed op. Only the untrusted user handler is
/// wrapped — a panic in internal code (access gate, encode) still propagates so
/// it surfaces as a bug rather than a silenced client error.
async fn catch_handler_panic<T>(fut: impl std::future::Future<Output = T>) -> Result<T, String> {
    use futures_util::future::FutureExt;
    std::panic::AssertUnwindSafe(fut)
        .catch_unwind()
        .await
        .map_err(|_| "source handler panicked".to_string())
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct OpState {
    intro: Arc<FieldDesc>,
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
    /// server only emits what was requested. For a PUT_GET op this is
    /// the GET-leg (`getField`) mask — the readback projection used by
    /// `putGet` and `getGet`.
    mask: BitSet,
    /// PUT-leg (`putField`) mask for a PUT_GET op, used by the `getPut`
    /// sub-command to project the put-side structure's current value
    /// (pvDatabaseCPP `ChannelPutGetLocal` builds separate putField /
    /// getField copies, channelLocal.cpp). `None` for every other op
    /// kind, which has a single selection mask in [`Self::mask`].
    put_mask: Option<BitSet>,
    /// Pipeline credit window. pvxs `MonitorOp::window` —
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
    /// MONITOR pause flag. pvxs subcmd `0x04` (without the
    /// `0x40` start bit) signals "stop emitting events but keep the
    /// op alive"; pvxs `Subscription::pause(true)` uses this. The
    /// subscriber task checks before emit and skips when `true`.
    /// Pulsed via the same notify as the credit window so the loop
    /// wakes on resume.
    monitor_paused: Arc<std::sync::atomic::AtomicBool>,
    /// pulsed on RESUME so the subscriber loop wakes and
    /// flushes the value it squashed while paused — for both pipelined
    /// and non-pipelined monitors (the credit `monitor_window_notify`
    /// is `None` for non-pipelined, so resume needs its own wake). pvxs
    /// keeps posting into the monitor queue while Idle and drains it on
    /// START (`servermon.cpp:211-220,671-688`); the Rust equivalent is
    /// "hold the squashed latest value, emit on resume".
    monitor_resume: Arc<tokio::sync::Notify>,
    /// Per-PV pipeline-window watermark levels
    /// `(low, high)` for this monitor op (from
    /// [`crate::server_native::ChannelSource::monitor_watermarks`]), or
    /// `None` when the source exposes none. Captured at INIT so both the
    /// subscriber loop (LOW, on DATA drain) and the per-connection ACK
    /// dispatch (HIGH, on credit refill) read the same levels.
    monitor_wm: Option<(usize, usize)>,
    /// Hysteresis state + ordering counter shared between the subscriber
    /// loop and the ACK dispatch so the HIGH (resume) and LOW (pause)
    /// watermark callbacks fire once per crossing. Its PARITY is the
    /// state — odd = window above `high` (resume pending), even = at/below
    /// `low` (pause pending) — and its VALUE is a strictly-monotonic
    /// firing sequence minted in the same atomic transition that decides
    /// each crossing (see [`cross_watermark`]). Starts `1` (odd: the
    /// window begins full, above high).
    ///
    /// HIGH MUST fire from the ACK path. A gateway source
    /// pauses its single upstream monitor on LOW; while paused no further
    /// events arrive, so firing HIGH from the event loop (the pre-FR-11
    /// behaviour) could never re-fire — the upstream would stay paused
    /// forever. pvxs fires `onHighMark` from the ACK handler
    /// (`servermon.cpp:653-666`).
    ///
    /// the value is also threaded to the source's
    /// `notify_watermark` as an ordering token so a gateway applying
    /// pause/resume out of process can discard a re-ordered command —
    /// closing a residual race where a resume could be lost behind a
    /// stale pause across the two firing tasks.
    monitor_wm_seq: Arc<std::sync::atomic::AtomicU64>,
    /// process-unique id for THIS monitor
    /// op, minted once at INIT via [`next_op_id`]. A fanout gateway shares
    /// one upstream monitor across N downstream ops of the same
    /// PV+credential; it reference-counts their pause votes keyed on this
    /// id so a fast co-subscriber's crossings cannot shadow a slow one's
    /// pause, and a torn-down op's vote is withdrawn by id. Read by BOTH
    /// the ACK-dispatch HIGH and the subscriber-loop LOW so both votes
    /// carry the same op identity.
    monitor_op_id: u64,
    /// Server-side filter chain decoded from
    /// `record._options._filter` (a JSON string carrying the same
    /// channel-filter syntax CA uses: `{"dbnd":{"d":0.5},...}`). The
    /// monitor subscriber task wraps each emitted event through
    /// `apply()` before building the wire payload — filters that drop
    /// the event cause the iteration to continue without sending.
    /// Empty chain (the default) is a no-op.
    monitor_filters: Arc<epics_base_rs::server::database::filters::FilterChain>,
    /// full INIT pvRequest value (decoded). PVA PUT INIT
    /// carries per-operation options (`record._options.process` /
    /// `block`, etc.) that the data-phase payload does NOT carry.
    /// We stash the value here at INIT so the data-phase PUT can
    /// attach it to the [`ChannelContext`](crate::server_native::ChannelContext) forwarded to the source,
    /// letting sources like the QSRV bridge honor process/block
    /// without re-parsing the value (where they no longer live).
    pv_request: Option<PvField>,
    /// event-affecting MONITOR pvRequest options
    /// (`pipeline` / `queueSize` / `_filter`) decoded at INIT. Passed
    /// to the source's `subscribe_*_checked_opts` at START so a
    /// fanout source (PVA gateway) can reject options it cannot honor
    /// transparently across a shared upstream monitor.
    monitor_options: crate::server_native::source::MonitorOptions,
    /// abort guard for the spawned data-phase task (GET /
    /// PUT / RPC / PUT_GET / PROCESS exec). When a DESTROY_REQUEST
    /// arrives, dropping the Op removes this Arc; once the last clone
    /// is dropped, `AbortOnDrop::drop()` fires and the task is
    /// cancelled, preventing a stale response from reaching the
    /// client after DESTROY. Idle (INIT-only) and MONITOR ops leave
    /// this as `None`.
    data_task_abort: Option<Arc<AbortOnDrop>>,
    /// single owner of this MONITOR op's Executing<->Idle edge
    /// (see [`MonitorStartControl`]). `Some` once the subscriber task is
    /// spawned; `None` for GET/PUT/RPC ops and for a MONITOR op that has
    /// been INIT'd but never STARTed. Dropping the `OpState` (DESTROY /
    /// channel destroy / connection reset) drops this Arc; the last drop
    /// fires the terminal `notify_monitor_start(false)` iff still
    /// executing.
    monitor_start_ctl: Option<Arc<MonitorStartControl>>,
    /// in-flight EXEC state for non-monitor (GET/PUT/RPC/PUT_GET/
    /// PROCESS) ops. `Idle` at INIT; flipped to `Executing` by [`begin_exec`]
    /// when a data-phase EXEC is accepted, and back to `Idle` by
    /// [`apply_exec_finish`] when the spawned task's response is sent. A
    /// second EXEC while `Executing` is ignored rather than aborting the
    /// in-flight task (pvxs `serverget.cpp:467-476`/`:511-514`). MONITOR ops
    /// leave this `Idle` (their lifecycle is `monitor_start_ctl`).
    exec_state: ExecState,
    /// set when a data-phase EXEC carried the last-request bit
    /// (`subcmd & 0x10`). pvxs records `op->lastRequest` while the op stays
    /// `Executing` in `opByIOID` and only `cleanup()`s it *after* the reply
    /// is serialized (`serverget.cpp:111-114`). The op therefore keeps its
    /// IOID reserved until [`apply_exec_finish`] observes the spawned reply
    /// task complete, so a re-INIT racing the slow source cannot reuse the
    /// IOID while the first reply is still in flight.
    last_request: bool,
}

/// atomically cross a pipeline-window watermark and mint
/// an ordering token in ONE transition over `state`
/// ([`OpState::monitor_wm_seq`]).
///
/// The counter's parity is the hysteresis state — odd = window above
/// `high` (upstream-resume pending), even = at/below `low`
/// (upstream-pause pending) — and its value is a strictly-monotonic
/// firing sequence. Returns `Some(seq)` exactly once per real crossing
/// (so HIGH/LOW still fire once per edge), with the post-crossing value
/// as the token; `None` when already in the requested state.
///
/// Minting the token in the SAME CAS that decides the crossing is what
/// lets the gateway apply pause/resume in true firing order: a token
/// assigned *after* the decision could be re-ordered across the two
/// firing tasks (subscriber emission loop for LOW, ACK dispatch for
/// HIGH) and leave an upstream wrongly — and permanently — paused.
/// `want_above == true` is the HIGH (resume) crossing, `false` the LOW
/// (pause) crossing.
fn cross_watermark(state: &std::sync::atomic::AtomicU64, want_above: bool) -> Option<u64> {
    use std::sync::atomic::Ordering;
    let mut cur = state.load(Ordering::Acquire);
    loop {
        let is_above = cur & 1 == 1;
        if is_above == want_above {
            return None;
        }
        let next = cur + 1;
        match state.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(next),
            Err(observed) => cur = observed,
        }
    }
}

/// Single owner of a monitor's pipeline-credit accounting. Holds the
/// per-op window state and watermark plumbing so EVERY monitor DATA send
/// site consumes its credit through one place.
///
/// pvxs `servermon.cpp:192` decrements `window` after every enqueued
/// DATA frame — the initial snapshot included (it is the first post
/// delivered once `state==Executing`) — and fires `onLowMark` on the
/// above→below crossing. Splitting that accounting across the
/// initial-snapshot send and the update loop (where only the loop
/// decremented) let the client's window drift to `queueSize + 1`: the
/// client counts the initial frame against its queue, the server did
/// not, so the server sent one DATA frame more than the client could
/// hold before the first ACK. Both send sites now route through
/// [`Self::take`] exactly once before sending.
struct MonitorPipelineCredit<'a> {
    /// `None` for a non-pipeline monitor — [`Self::take`] is then a
    /// no-op (mpsc backpressure stays the only gate, as before).
    window: Option<&'a Arc<std::sync::atomic::AtomicU32>>,
    window_notify: Option<&'a Arc<tokio::sync::Notify>>,
    /// INIT-clamped `(low, high)` watermark levels, or `None` when the
    /// source declares no pipeline watermarks.
    wm_levels: Option<(usize, usize)>,
    wm_seq: &'a Arc<std::sync::atomic::AtomicU64>,
    monitor_op_id: u64,
    src: &'a DynSource,
    pv_name: &'a str,
    mon_ctx: &'a crate::server_native::source::ChannelContext,
}

impl MonitorPipelineCredit<'_> {
    /// True iff a DATA frame may be sent right now — the window holds a
    /// credit, or this is a non-pipeline monitor (no window at all).
    ///
    /// This is pvxs `maybeReply`'s `(!op->pipeline || op->window)`
    /// (`servermon.cpp:79-83`, echoed in `doReply` at `:143`): an exhausted
    /// window suppresses the REPLY, and nothing else. It must NOT be awaited
    /// inside the event loop's select — pvxs keeps calling `doPost`, so a
    /// stalled pipelined client goes on squashing into the negotiated queue.
    /// Awaiting here instead stopped polling the source, buffering
    /// `channel_capacity + limit` distinct updates and delivering them all on
    /// resume.
    fn available(&self) -> bool {
        let Some(w) = self.window else {
            return true;
        };
        w.load(std::sync::atomic::Ordering::Relaxed) > 0
    }

    /// Register for the next ACK refill, BEFORE the caller reads the window.
    ///
    /// Ordering is load-bearing: the ACK path increments the window and then
    /// calls `Notify::notify_waiters()`, which stores NO permit. A waiter
    /// registered after the window read would miss a refill that landed in
    /// between and park forever with credit in hand. `enable()` registers
    /// eagerly (`Notified` does not register until first polled), so arming
    /// first and reading the window second cannot lose the wake-up in either
    /// interleaving.
    ///
    /// `None` for a non-pipeline monitor — it is never credit-blocked, so it
    /// never waits on this.
    fn arm_refill(&self) -> Option<std::pin::Pin<Box<tokio::sync::futures::Notified<'_>>>> {
        let n = self.window_notify?;
        let mut notified = Box::pin(n.notified());
        notified.as_mut().enable();
        Some(notified)
    }

    /// Consume one credit for the DATA frame about to be sent and fire the LOW
    /// watermark on the above→below crossing. A no-op for a non-pipeline
    /// monitor (`window` is `None`).
    ///
    /// Must be called exactly once per monitor DATA frame, AFTER the pause /
    /// filter gates (a held or filtered event produces no wire frame, so it
    /// must not consume a slot) and only under [`Self::available`] — this is
    /// the ONLY site that decrements the window (the ACK path only adds), so
    /// the credit checked in the emit gate is still there.
    fn take(&self) {
        use std::sync::atomic::Ordering;
        let Some(w) = self.window else {
            return;
        };
        // Saturating decrement, spelled as the CAS loop `fetch_update` was
        // compiling to anyway. Not the deprecation's suggested rename:
        // `try_update` — and its infallible sibling `update` — are unstable
        // (`atomic_try_update`, rust#135894), so on this workspace's pinned
        // 1.94 toolchain neither is reachable. `fetch_sub` is not the answer
        // either: it wraps to `u32::MAX` at zero, which would hand the emit
        // gate four billion credits at exactly the moment the window is
        // empty. Same shape `epics-ca-rs` uses to drain `pending_frames`
        // (`client/transport.rs`), and for the same missing primitive.
        let mut cur = w.load(Ordering::Relaxed);
        while let Err(observed) = w.compare_exchange_weak(
            cur,
            cur.saturating_sub(1),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            cur = observed;
        }
        // LOW fires when consuming this credit drained the
        // window to `<= low` (pvxs `onLowMark`).
        // `cross_watermark` checks-and-marks the above→below crossing AND
        // mints the ordering token in one CAS, returning `Some(seq)`
        // exactly on the edge so LOW fires once per crossing even though
        // the companion HIGH lives on the ACK dispatch path.
        if let Some((lo, _hi)) = self.wm_levels {
            let w_after = w.load(Ordering::Relaxed) as usize;
            if w_after <= lo
                && let Some(seq) = cross_watermark(self.wm_seq, false)
            {
                self.src.notify_watermark(
                    self.pv_name,
                    self.mon_ctx,
                    crate::server_native::source::WatermarkEvent {
                        op_id: self.monitor_op_id,
                        seq,
                        kind: crate::server_native::source::WatermarkKind::Pause,
                    },
                );
            }
        }
    }
}

/// Await a pipeline-credit refill. Only ever polled from the emit loop's
/// credit arm, which runs solely when the window is exhausted — and an
/// exhausted window implies a pipeline monitor, so `refill` is `Some` there.
/// A non-pipeline monitor is never credit-blocked; `pending()` makes that arm
/// inert for it rather than spinning the loop.
async fn wait_credit_refill(
    refill: Option<std::pin::Pin<Box<tokio::sync::futures::Notified<'_>>>>,
) {
    match refill {
        Some(n) => n.await,
        None => std::future::pending().await,
    }
}

/// Process-unique monitor-op id. A fanout gateway shares one upstream
/// monitor across N downstream ops and reference-counts their pause votes
/// keyed on this id; a global monotonic counter keeps ids distinct across
/// reconnects (a per-(sid,ioid) tuple would recycle). Wraps at u64::MAX —
/// not reachable in any real deployment.
fn next_op_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// finalizer that withdraws this monitor op's
/// upstream pause vote when its subscriber task ends — for ANY reason
/// (normal completion, early `return`, or `AbortOnDrop` cancelling the
/// task on DESTROY_REQUEST / channel teardown / disconnect).
///
/// Invariant (MUST): a torn-down op casts no pause vote on the shared
/// upstream. The LOW vote fires from the subscriber emission loop; the
/// matching HIGH (release) fires from the ACK-dispatch path on the
/// *connection* task. When the op is destroyed the subscriber task is
/// aborted and that ACK path stops servicing the now-removed ioid, so a
/// HIGH that would have released the op's last pause vote can never run.
/// Absent this finalizer the gateway's *shared* `UpstreamEntry` (one
/// upstream monitor fanned to N downstream subscribers of the same
/// PV+credential) reference-counts a pause vote with no live owner to
/// withdraw it — if it was the last op holding data flowing, every
/// co-subscriber is starved permanently. This is the single owner of the
/// "withdraw-on-exit" transition: it lives in the subscriber task and so
/// covers every exit path by construction, rather than scattering a
/// withdraw at each teardown site.
///
/// On drop it fires [`WatermarkKind::Withdraw`](crate::server_native::source::WatermarkKind::Withdraw) unconditionally for this
/// op_id. The gateway removes the op's vote and recomputes the aggregate:
/// if this op's vote was the only thing keeping the upstream paused (or
/// keeping it from pausing), the aggregate edge fires the corresponding
/// resume/pause; otherwise it is a no-op. A non-gateway source's
/// `notify_watermark` default ignores it. Idempotent and cheap (sync mpsc
/// send), so it is safe to run from `Drop`.
struct WatermarkWithdrawOnDrop {
    src: DynSource,
    pv_name: String,
    ctx: crate::server_native::source::ChannelContext,
    op_id: u64,
}

impl Drop for WatermarkWithdrawOnDrop {
    fn drop(&mut self) {
        self.src.notify_watermark(
            &self.pv_name,
            &self.ctx,
            crate::server_native::source::WatermarkEvent {
                op_id: self.op_id,
                seq: 0,
                kind: crate::server_native::source::WatermarkKind::Withdraw,
            },
        );
    }
}

/// Drives a per-op MONITOR gate ([`crate::server_native::source::
/// MonitorGate`], supplied by the source on its `SubscriptionSeed`) from
/// this op's executing state.
///
/// This used to be a task of its own, watching the same
/// `tokio::sync::watch` the subscriber loop already watches. It is now
/// owned by that loop: the loop reads the executing state once per
/// iteration and wakes on every edge (its `exec_rx.changed()` arm), so it
/// is already at the exact observation points the driver task duplicated —
/// the extra task bought a second observer, not a second observation.
///
/// Semantics carried over unchanged:
///
/// * The **first** application is unconditional (`applied: None`), which is
///   what made a STOP that fired before the driver started not get missed.
///   It is issued where the spawn used to be, before the loop is entered.
/// * Every later application is edge-only, coalescing to the latest
///   executing state — `set_active` is idempotent, so the net gate state
///   always matches `executing`.
/// * No gate (`None`) is the common case — only a QSRV db/group monitor
///   supplies one — and costs one `Option` check per iteration.
///
/// Lifetime: ends with the subscriber. Teardown then removes the backing
/// subscriptions along with the op (STOP=disable, teardown=remove), so no
/// final edge is owed after the loop exits — the driver task did not apply
/// one either; it simply ended when the watch sender dropped.
struct MonitorGateDriver {
    gate: Option<crate::server_native::source::MonitorGate>,
    /// Last state handed to the gate; `None` until the unconditional first
    /// application, which is what distinguishes "not yet applied" from
    /// "applied, and it was Idle".
    applied: Option<bool>,
}

impl MonitorGateDriver {
    fn new(gate: Option<crate::server_native::source::MonitorGate>) -> Self {
        Self {
            gate,
            applied: None,
        }
    }

    async fn apply(&mut self, executing: bool) {
        let Some(gate) = &self.gate else {
            return;
        };
        if self.applied == Some(executing) {
            return;
        }
        self.applied = Some(executing);
        gate.set_active(executing).await;
    }
}

/// single owner of one MONITOR op's Executing<->Idle edge.
/// pvxs fires `MonitorControlOp::onStart(bool)` once when a monitor
/// begins producing and once when it stops (`servermon.cpp:677-683`); we
/// mirror that through [`ChannelSource::notify_monitor_start`](crate::server_native::ChannelSource::notify_monitor_start), firing
/// only on a real edge so a gateway source can suspend its upstream
/// subscription while every downstream consumer is paused.
///
/// INVARIANT: `notify_monitor_start(true)` fires exactly once per
/// Idle->Executing edge and `notify_monitor_start(false)` exactly once
/// per Executing->Idle edge, where "Executing" means the subscriber is
/// producing (started and not paused). This struct is the ONLY caller of
/// `notify_monitor_start`. Every transition site — START spawn, MONITOR
/// PAUSE, MONITOR RESUME, CANCEL_REQUEST — routes through [`Self::set`],
/// which is edge-triggered. Every teardown path — DESTROY, channel
/// destroy, connection reset — drops the owning [`OpState`] (and thus
/// this struct), and [`Drop`] fires the terminal `false` iff still
/// executing. So a torn-down monitor can never leave the source
/// believing it is still producing, and a DESTROY following a
/// PAUSE/CANCEL does not double-fire `false`.
///
/// `executing` is an `AtomicBool` rather than a lock: every `set` is
/// driven from the single per-connection read loop (which processes this
/// op's START/PAUSE/RESUME/CANCEL/DESTROY frames in order), and `swap`
/// gives the edge test atomically.
struct MonitorStartControl {
    src: DynSource,
    pv_name: String,
    ctx: crate::server_native::source::ChannelContext,
    executing: std::sync::atomic::AtomicBool,
    /// Publishes each Executing<->Idle edge to this op's subscriber task,
    /// which drives the source-supplied [`crate::server_native::source::
    /// MonitorGate`] (QSRV `db_event_enable`/`db_event_disable`
    /// on STOP/RESUME). Sending the same edge `notify_monitor_start` fires,
    /// so the source-level pause-vote path and the per-op subscription gate
    /// stay in lockstep. On teardown this `Sender` drops with the op,
    /// ending the gate-driver task.
    exec_tx: tokio::sync::watch::Sender<bool>,
}

impl std::fmt::Debug for MonitorStartControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MonitorStartControl")
            .field("pv_name", &self.pv_name)
            .field(
                "executing",
                &self.executing.load(std::sync::atomic::Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl MonitorStartControl {
    fn new(
        src: DynSource,
        pv_name: String,
        ctx: crate::server_native::source::ChannelContext,
        exec_tx: tokio::sync::watch::Sender<bool>,
    ) -> Self {
        Self {
            src,
            pv_name,
            ctx,
            executing: std::sync::atomic::AtomicBool::new(false),
            exec_tx,
        }
    }

    /// Edge-triggered: fire `notify_monitor_start(desired)` and publish the
    /// edge to the per-op gate driver only when the executing state actually
    /// changes to `desired`.
    fn set(&self, desired: bool) {
        if self
            .executing
            .swap(desired, std::sync::atomic::Ordering::Relaxed)
            != desired
        {
            self.src
                .notify_monitor_start(&self.pv_name, &self.ctx, desired);
            // Publish to this op's gate driver. Ignore send
            // errors: a closed receiver means the subscriber task already
            // ended, in which case its backing subscriptions are gone too.
            let _ = self.exec_tx.send(desired);
        }
    }

    /// True iff the most recent edge set this monitor Executing — a real
    /// MONITOR START not yet followed by STOP/PAUSE/CANCEL. The subscriber's
    /// emit gate follows this same state via the `monitor_exec` watch; this
    /// accessor exists only so tests can observe the edge (the watch receiver
    /// is internal to the subscriber task), hence `#[cfg(test)]`.
    #[cfg(test)]
    fn is_executing(&self) -> bool {
        self.executing.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for MonitorStartControl {
    fn drop(&mut self) {
        // Terminal Executing->Idle for any teardown that did not PAUSE /
        // CANCEL first (DESTROY, channel destroy, connection reset).
        if *self.executing.get_mut() {
            *self.executing.get_mut() = false;
            self.src
                .notify_monitor_start(&self.pv_name, &self.ctx, false);
        }
    }
}

/// A monitor post that may be pvxs's **terminal** — its null `Value`.
///
/// pvxs's `doPost` (`servermon.cpp:270-283`) gates the append on
/// `(mon->queue.size() < mon->limit) || force || !val`, so a terminal is
/// ALWAYS `push_back`'d and grows the queue past `limit`; only a
/// non-terminal post can reach the squash branch. Both FIFOs the port
/// runs — the decoded [`crate::server_native::MonitorUpdate`] queue and
/// the raw-forward [`crate::server_native::RawMonitorEvent`] queue —
/// carry that boundary as `type_changed`, so the rule lives in
/// [`push_squash_monitor`] behind this trait rather than at each call
/// site: a caller cannot forget it.
trait MonitorPost {
    /// pvxs's `!val`.
    fn is_terminal(&self) -> bool;
}

impl MonitorPost for crate::server_native::MonitorUpdate {
    fn is_terminal(&self) -> bool {
        self.type_changed
    }
}

impl MonitorPost for crate::server_native::RawMonitorEvent {
    fn is_terminal(&self) -> bool {
        self.type_changed
    }
}

/// Push one monitor event into the bounded FIFO, squashing the newest into the
/// tail once the queue is full — the single producer rule covering both the
/// INIT->START and STOP->START "Idle, accruing" windows AND the Executing
/// burst. Models pvxs `servermon.cpp:270-287`:
///
/// * a **terminal** post ([`MonitorPost::is_terminal`], pvxs's `!val`) is
///   always appended, even past `limit` — pvxs delivers every queued update
///   and *then* the FINISH, so the terminal must never destroy a real one;
/// * otherwise a post is appended as a DISTINCT queue entry while the queue
///   holds fewer than `limit` entries;
/// * once full, a non-terminal post is coalesced into the queue tail
///   (unioning marked-leaf sets via [`coalesce_monitor_update`] on the
///   decoded path).
///
/// `limit` must be >= 1 so a tail always exists to squash into. Returns
/// whether an overflow squash happened (diagnostic only).
fn push_squash_monitor<T: MonitorPost>(
    pending: &mut std::collections::VecDeque<T>,
    ev: T,
    limit: usize,
    coalesce: impl Fn(T, T) -> T,
) -> bool {
    if ev.is_terminal() || pending.len() < limit {
        pending.push_back(ev);
        false
    } else {
        let tail = pending
            .pop_back()
            .expect("len >= limit >= 1 guarantees a tail to squash into");
        pending.push_back(coalesce(tail, ev));
        true
    }
}

/// The decoded monitor's bounded FIFO and the SINGLE owner of the enqueue
/// transition — pvxs `ServerMonitorControl::doPost` (`servermon.cpp:239-289`).
///
/// Invariant: **an update reaches the queue only if it would produce a
/// non-empty wire changed-bitset** (or it is the first post, or a terminal).
/// pvxs decides that BEFORE touching the queue:
///
/// ```text
/// bool real = mon->first;                       // always post the first update
/// if(real) mon->first = false;
/// else     real = testmask(val, mon->pvMask);   // else consider the mask
/// if(real || !val) { ...queue or squash...; maybeReply(); }
/// ```
///
/// A masked-out update is DROPPED, never queued: it neither occupies a slot
/// in the negotiated FIFO nor coalesces a real update out of the tail. The
/// port used to push every arrival straight into the FIFO, so an update whose
/// marked leaves lay entirely outside the client's pvRequest mask both framed
/// an empty-bitset frame and evicted a real update under back-pressure — the
/// squash CONTENTS differed, not just the frame count.
///
/// [`Self::seed`] carries pvxs's `first`: the connect-time seed IS the first
/// post, so it is exempt from the mask test (and clears `first`). A source
/// with no seed leaves `first` set, so ITS first stream event is the exempt
/// one — exactly `MonitorOp::first` ("set until first update queued").
struct MonitorQueue<'a> {
    pending: std::collections::VecDeque<crate::server_native::MonitorUpdate>,
    /// The ONE negotiated squash limit (`MonitorOp::limit`).
    limit: usize,
    /// pvxs `MonitorOp::first` — set until the first update is queued.
    first: bool,
    intro: &'a FieldDesc,
    /// The op's pvRequest selection mask (`MonitorOp::pvMask`).
    mask: &'a BitSet,
    /// The wire changed-bitset a `marked: None` post frames — pvxs's
    /// fully-marked `Value` intersected with `pvMask`. It depends only on
    /// `(intro, mask)`, so it is computed once here rather than per post.
    /// Empty ⟺ the request selected no leaf, and then no unmarked post is
    /// `real`.
    unmarked_changed: BitSet,
}

impl<'a> MonitorQueue<'a> {
    fn new(limit: usize, intro: &'a FieldDesc, mask: &'a BitSet) -> Self {
        Self {
            pending: std::collections::VecDeque::new(),
            limit: limit.max(1),
            first: true,
            intro,
            mask,
            unmarked_changed: crate::pvdata::encode::canonical_changed_bitset(intro, mask),
        }
    }

    /// Queue the connect-time seed as pvxs's first post: exempt from the mask
    /// test, and it clears `first` so every later arrival is tested. The seed
    /// carries the leaves the source declared it assigned
    /// ([`crate::server_native::source::SourceRead`]), so the START frame is
    /// framed by the same rule as every update.
    fn seed(&mut self, initial: crate::server_native::source::SourceRead) {
        self.first = false;
        self.pending
            .push_back(crate::server_native::MonitorUpdate::from(initial));
    }

    /// pvxs `doPost`. Returns whether the update was queued (`false` = dropped
    /// by the mask test, i.e. `real == false`).
    fn push(&mut self, ev: crate::server_native::MonitorUpdate) -> bool {
        if !self.real(&ev) {
            return false;
        }
        self.first = false;
        push_squash_monitor(&mut self.pending, ev, self.limit, coalesce_monitor_update);
        true
    }

    /// pvxs's `real || !val`: the first post and a terminal (pvxs's null Value
    /// — here the `type_changed` boundary, which MUST survive to become the
    /// MONITOR FINISH) always queue; anything else must pass `testmask`
    /// (`pvrequest.cpp:73-92`) — at least one marked bit inside `pvMask`.
    ///
    /// `testmask` is a LEAF test, on both arms. It scans `store[idx].valid`,
    /// and `Value::mark` (`data.cpp:256-270`) sets `valid` on the marked field
    /// and on the *enclosing tops* of a struct-array element — never on a
    /// parent `Struct` node. So a `pvMask` covering only structure bits can
    /// never satisfy it, however much the source marked: `field(alarm.bogus)`
    /// selects `{0, alarm}` (`request2mask` matches the existing `alarm`
    /// struct, finds no `alarm.bogus`, and pre-sets the always-permitted bit
    /// 0), and pvxs stays silent for the life of that subscription.
    ///
    /// The gate is therefore the frame's own changed-bitset — the SAME value
    /// the payload builder about to serialize this update computes, on either
    /// arm ([`read_changed_bitset`]: `marked_wire_changed_bitset` for a
    /// declared leaf set, `canonical_changed_bitset` for a wholly-changed
    /// post). That makes
    /// `gate == wire` an invariant: an admitted update always frames a
    /// non-empty changed-bitset, and a leafless mask frames none because it
    /// queues none.
    fn real(&self, ev: &crate::server_native::MonitorUpdate) -> bool {
        if self.first || ev.type_changed {
            return true;
        }
        match ev.marked.as_ref() {
            Some(paths) => {
                !crate::pvdata::encode::marked_wire_changed_bitset(self.intro, paths, self.mask)
                    .is_empty()
            }
            None => !self.unmarked_changed.is_empty(),
        }
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    fn pop(&mut self) -> Option<crate::server_native::MonitorUpdate> {
        self.pending.pop_front()
    }
}

/// Inputs to [`spawn_monitor_subscriber`]. A cohesive bundle of the per-op
/// monitor state captured at INIT (read out of the just-inserted `OpState`)
/// plus the connection-scope handles the subscriber task needs — one struct
/// keeps the INIT call site and the helper signature readable.
struct MonitorSubscriberArgs {
    sid: u32,
    ioid: u32,
    pv_name: String,
    intro: Arc<FieldDesc>,
    mask: BitSet,
    /// Per-channel writer (clone of the connection `ChannelTx`).
    tx: ChannelTx,
    src: DynSource,
    /// Per-op squash threshold (client `record._options.queueSize` or the
    /// server default), computed at INIT.
    queue_depth: usize,
    high_watermark: usize,
    /// Credential + INIT-pvRequest context for subscribe / get / ACL.
    mon_ctx: crate::server_native::source::ChannelContext,
    window: Option<Arc<std::sync::atomic::AtomicU32>>,
    window_notify: Option<Arc<tokio::sync::Notify>>,
    filters: Arc<epics_base_rs::server::database::filters::FilterChain>,
    monitor_options: crate::server_native::source::MonitorOptions,
    wm_seq: Arc<std::sync::atomic::AtomicU64>,
    monitor_op_id: u64,
    wm_levels: Option<(usize, usize)>,
    mon_fin_tx: mpsc::UnboundedSender<MonitorFinished>,
    /// Connection's live outbound byte-order cell.
    out_order: Arc<std::sync::atomic::AtomicBool>,
}

/// Spawn the per-op MONITOR subscriber task at INIT (pvxs `onSubscribe`
/// registers the upstream at INIT — `servermon.cpp:591`), returning the task
/// abort handle + the Executing<->Idle edge owner so the INIT branch can
/// install them into the op's `OpState`.
///
/// Invariant — one bounded FIFO per monitor. The PRODUCER drains the
/// source `updates` channel into `pending` (squash-to-`queue_limit`) from the
/// moment of subscribe, i.e. from INIT, so posts arriving in the INIT->START
/// window are accrued, not lost. The CONSUMER emits `pending` to the wire one
/// frame per pipeline credit, but ONLY while Executing (after a MONITOR START,
/// not STOPped). INIT->START and STOP->START are the same "Idle, accruing"
/// state. This retires the old `held` single-cell pause buffer: a STOP->START
/// now delivers up to `queueSize` (was: latest only).
///
/// Executing state is read from the op's `monitor_exec` watch (set by
/// [`MonitorStartControl`] on every START/STOP edge). Teardown: the returned
/// `TaskHandle` is wrapped in the op's `monitor_abort` `AbortOnDrop`; dropping
/// the `OpState` (DESTROY / channel destroy / disconnect — including
/// DESTROY-before-START and never-START) aborts the task, dropping `rx` and the
/// source subscription handle, releasing the upstream. The `MonitorFinishGuard`
/// installed as the first task statement reports terminal removal on every exit.
fn spawn_monitor_subscriber(
    args: MonitorSubscriberArgs,
) -> (
    epics_base_rs::runtime::task::TaskHandle<()>,
    Arc<MonitorStartControl>,
) {
    let MonitorSubscriberArgs {
        sid,
        ioid,
        pv_name,
        intro: intro_clone,
        mask: mask_clone,
        tx: tx_clone,
        src,
        queue_depth,
        high_watermark,
        mon_ctx,
        window,
        window_notify,
        filters,
        monitor_options,
        wm_seq,
        monitor_op_id,
        wm_levels: wm_levels_init,
        mon_fin_tx,
        out_order: out_order_mon,
    } = args;

    // Credential-scoped context (no pv_request) for the start-control /
    // watermark paths — a fanout gateway scopes the upstream suspend/resume to
    // the firing credential's cache layer.
    let credential_ctx = crate::server_native::source::ChannelContext {
        pv_request: None,
        ..mon_ctx.clone()
    };

    // Per-op executing-state watch. `MonitorStartControl` publishes each
    // Executing<->Idle edge here and the subscriber loop is its ONLY reader:
    // the loop uses it both as its emit gate and — via `MonitorGateDriver` —
    // to drive the source's optional `MonitorGate`. Starts `false` (Idle);
    // the first MONITOR START flips it to `true`.
    let (monitor_exec_tx, monitor_exec_rx) = tokio::sync::watch::channel(false);
    let start_ctl = Arc::new(MonitorStartControl::new(
        src.clone(),
        pv_name.clone(),
        credential_ctx,
        monitor_exec_tx,
    ));

    let total_bits = intro_clone.total_bits();
    // Raw fast path eligibility (same predicate as the legacy spawn): 1:1 mask,
    // no pipeline window, no server filter chain.
    let raw_path_eligible = mask_clone.count() == total_bits
        && mask_clone.size() >= total_bits
        && window.is_none()
        && filters.is_empty();

    let mon_fin = MonitorFinished {
        sid,
        ioid,
        op_id: monitor_op_id,
    };
    // A single receiver, moved into the subscriber task. The second clone
    // this used to make belonged to the `MonitorGate` driver task, which the
    // loop now owns directly.
    let loop_exec_rx = monitor_exec_rx;

    let join = epics_base_rs::runtime::task::spawn(async move {
        let _fin_guard = MonitorFinishGuard {
            tx: mon_fin_tx,
            fin: mon_fin,
        };
        let order_now = || {
            if out_order_mon.load(std::sync::atomic::Ordering::Relaxed) {
                ByteOrder::Big
            } else {
                ByteOrder::Little
            }
        };
        let mut exec_rx = loop_exec_rx;
        let queue_limit = queue_depth.max(1);

        // Version capture must precede the check (an older-or-equal captured
        // version => loop re-checks on the next event after a reload).
        let mon_acl_version_at_subscribe_cell = Arc::new(std::sync::atomic::AtomicU64::new(
            src.access_gate().acl_version(),
        ));
        let mon_checked = src
            .access_gate()
            .check_with_roles(
                &pv_name,
                &mon_ctx.host,
                &mon_ctx.account,
                &mon_ctx.roles,
                &mon_ctx.method,
                &mon_ctx.authority,
            )
            .await;

        // ---------------- RAW FAST PATH ----------------
        let raw_seed = if raw_path_eligible {
            src.subscribe_raw_seeded(
                mon_checked.clone(),
                mon_ctx.clone(),
                monitor_options.clone(),
            )
            .await
        } else {
            None
        };
        // Emit whatever the source recorded while opening the subscription
        // (pvxs `singlesource.cpp:129` — `record._options.DBE` selecting an
        // empty mask), whether or not it produced a seed. pvxs logs from
        // inside `onSubscribe`, i.e. before its `connect()` INIT reply; here
        // the INIT reply is enqueued by the read loop that spawned this task,
        // so the frame follows that reply. Same ioid, level and text — only
        // the position relative to the INIT reply differs.
        flush_remote_log(&mon_ctx.log, ioid, order_now(), &tx_clone).await;
        if let Some(seed_raw) = raw_seed {
            let crate::server_native::source::SubscriptionSeed {
                initial: seed_raw_initial,
                updates: mut rx_raw,
                // raw fast path is the fanout gateway, gated per (name, ctx) via
                // `notify_monitor_start`, not per op — no per-op gate.
                on_start: _,
            } = seed_raw;
            // Revalidate ACL before seeding (a reload between subscribe and here
            // could flip to NoAccess). Unlike the source-close FINISH in the loop
            // below, this ACL-deny FINISH is deliberately NOT Executing-gated: an
            // access revocation is a security event that closes the monitor
            // promptly even while Idle (approved INIT-surfacing shift + pvxs
            // prompt-close-on-revocation), whereas a lifecycle source-close holds
            // backlog+finish until START. Do NOT "align" this to hold — that would
            // keep an ACL-revoked subscription alive.
            let live_v0 = src.access_gate().acl_version();
            if live_v0
                != mon_acl_version_at_subscribe_cell.load(std::sync::atomic::Ordering::Acquire)
            {
                if src
                    .revalidate_read(&pv_name, mon_ctx.clone())
                    .await
                    .is_none()
                {
                    let finish = build_monitor_finish(ioid, order_now());
                    let _ = tx_clone.send(finish).await;
                    return;
                }
                mon_acl_version_at_subscribe_cell
                    .store(live_v0, std::sync::atomic::Ordering::Release);
            }
            // The connect-time seed (a decoded snapshot, emitted cooked)
            // and the accrued raw window are both Executing-gated; the seed is
            // emitted first, ahead of the backlog.
            let mut seed_cooked: Option<crate::server_native::source::SourceRead> =
                seed_raw_initial;
            let mut pending: std::collections::VecDeque<crate::server_native::RawMonitorEvent> =
                std::collections::VecDeque::new();
            let mut source_open = true;
            loop {
                let executing = *exec_rx.borrow_and_update();
                let has_work = seed_cooked.is_some() || !pending.is_empty();
                // Executing-gated terminal FINISH (see the decoded path): an
                // Idle/never-STARTed monitor holds the backlog and the finish
                // until a later START rather than emitting FINISH on source-close,
                // matching pvxs servermon.cpp:82,142-154. Teardown still aborts
                // the waiting task.
                if !source_open && executing && !has_work {
                    break;
                }
                tokio::select! {
                    biased;
                    r = rx_raw.recv(), if source_open => {
                        match r {
                            Some(ev) => {
                                // The cooked seed occupies one FIFO slot until the
                                // consumer emits it, so while it is pending the raw
                                // backlog is bounded to queue_limit-1 — keeping the
                                // total (seed + raw) at queueSize, matching the decoded
                                // path where the seed IS pending[0]. Once the seed is
                                // emitted the raw bound relaxes to queue_limit.
                                //
                                // Exception at queue_limit == 1: the `.max(1)` floor
                                // keeps one raw slot even while the seed is pending, so
                                // the seed plus one raw event briefly coexist (a
                                // transient +1 that relaxes the moment the seed emits).
                                // The raw seed is a decoded snapshot and cannot coalesce
                                // into a raw event, so unlike the decoded path it cannot
                                // share pending[0]. This is not client-reachable — a
                                // client queueSize < 2 is rejected at INIT, so
                                // queue_limit == 1 requires a non-default
                                // monitor_queue_depth == 1.
                                let raw_cap = queue_limit
                                    .saturating_sub(seed_cooked.is_some() as usize)
                                    .max(1);
                                push_squash_monitor(&mut pending, ev, raw_cap, |_old, new| new);
                                while let Ok(e) = rx_raw.try_recv() {
                                    push_squash_monitor(&mut pending, e, raw_cap, |_old, new| new);
                                }
                            }
                            None => source_open = false,
                        }
                    }
                    _ = exec_rx.changed() => {}
                    _ = std::future::ready(()), if executing && has_work => {
                        if let Some(initial) = seed_cooked.take() {
                            // Per-event ACL recheck on policy reload — the seed is
                            // emitted through the same gate as every backlog event
                            // (below), so an ACL reload during the idle window that
                            // revokes read suppresses the raw seed too, symmetric
                            // with the decoded path where the seed is pending[0] and
                            // is rechecked when popped.
                            let live_v = src.access_gate().acl_version();
                            if live_v
                                != mon_acl_version_at_subscribe_cell
                                    .load(std::sync::atomic::Ordering::Acquire)
                            {
                                if src.revalidate_read(&pv_name, mon_ctx.clone()).await.is_none() {
                                    let finish = build_monitor_finish(ioid, order_now());
                                    let _ = tx_clone.send(finish).await;
                                    return;
                                }
                                mon_acl_version_at_subscribe_cell
                                    .store(live_v, std::sync::atomic::Ordering::Release);
                            }
                            let payload = build_monitor_payload(
                                ioid,
                                &intro_clone,
                                &initial.value,
                                initial.marked.as_deref(),
                                &mask_clone,
                                order_now(),
                            );
                            if tx_clone.send(payload).await.is_err() {
                                return;
                            }
                            continue;
                        }
                        let ev = pending.pop_front().expect("has_work && no seed => pending non-empty");
                        if ev.type_changed {
                            let finish = build_monitor_finish(ioid, order_now());
                            let _ = tx_clone.send(finish).await;
                            return;
                        }
                        let live_v = src.access_gate().acl_version();
                        if live_v
                            != mon_acl_version_at_subscribe_cell
                                .load(std::sync::atomic::Ordering::Acquire)
                        {
                            if src.revalidate_read(&pv_name, mon_ctx.clone()).await.is_none() {
                                let finish = build_monitor_finish(ioid, order_now());
                                let _ = tx_clone.send(finish).await;
                                return;
                            }
                            mon_acl_version_at_subscribe_cell
                                .store(live_v, std::sync::atomic::Ordering::Release);
                        }
                        let payload = match raw_monitor_frame(ioid, &intro_clone, &ev, order_now()) {
                            RawMonitorFrame::Forward(p) => p,
                            RawMonitorFrame::Terminate { frame, reason } => {
                                debug!(
                                    pv = %pv_name,
                                    error = %reason,
                                    "Raw monitor reencode failed — terminating monitor with error"
                                );
                                let _ = tx_clone.send(frame).await;
                                return;
                            }
                        };
                        if tx_clone.send(payload).await.is_err() {
                            return;
                        }
                    }
                }
            }
            let finish = build_monitor_finish(ioid, order_now());
            let _ = tx_clone.send(finish).await;
            return;
        }

        // ---------------- DECODED PATH ----------------
        let seed = src
            .subscribe_seeded(
                mon_checked.clone(),
                mon_ctx.clone(),
                monitor_options.clone(),
            )
            .await;
        // Same source-diagnostic drain as the raw path above: the source may
        // have recorded a `record._options` warning while opening this
        // subscription, and it must reach the client even when the
        // subscription itself failed to open.
        flush_remote_log(&mon_ctx.log, ioid, order_now(), &tx_clone).await;
        let Some(seed) = seed else {
            return;
        };
        let crate::server_native::source::SubscriptionSeed {
            initial: seed_initial,
            updates: mut rx,
            on_start: seed_on_start,
        } = seed;
        // Owned by this loop rather than a task of its own; the
        // unconditional first application happens here, where the spawn
        // used to be, so a STOP that fired before the subscription opened
        // is still not missed. `borrow` (not `borrow_and_update`) leaves the
        // loop's own seen-state alone, so an edge landing between here and
        // the first iteration is still delivered to `exec_rx.changed()`.
        let mut gate_driver = MonitorGateDriver::new(seed_on_start);
        // Bound to a local so the watch `Ref` is dropped before the await —
        // it is not `Send`, and this future is spawned.
        let executing_now = *exec_rx.borrow();
        gate_driver.apply(executing_now).await;
        let mut queue_over_high = false;
        let wm_levels = wm_levels_init;
        let credit = MonitorPipelineCredit {
            window: window.as_ref(),
            window_notify: window_notify.as_ref(),
            wm_levels,
            wm_seq: &wm_seq,
            monitor_op_id,
            src: &src,
            pv_name: &pv_name,
            mon_ctx: &mon_ctx,
        };
        let _wm_withdraw_guard = wm_levels.is_some().then(|| {
            let mut ctx = mon_ctx.clone();
            ctx.pv_request = None;
            WatermarkWithdrawOnDrop {
                src: src.clone(),
                pv_name: pv_name.clone(),
                ctx,
                op_id: monitor_op_id,
            }
        });
        {
            // ACL-deny FINISH is prompt (not Executing-gated) — see the raw path's
            // pre-loop revalidate: a security revocation closes the monitor even
            // while Idle, unlike the lifecycle source-close FINISH which holds
            // backlog+finish until START.
            let live_v0 = src.access_gate().acl_version();
            if live_v0
                != mon_acl_version_at_subscribe_cell.load(std::sync::atomic::Ordering::Acquire)
            {
                if src
                    .revalidate_read(&pv_name, mon_ctx.clone())
                    .await
                    .is_none()
                {
                    let finish = build_monitor_finish(ioid, order_now());
                    let _ = tx_clone.send(finish).await;
                    return;
                }
                mon_acl_version_at_subscribe_cell
                    .store(live_v0, std::sync::atomic::Ordering::Release);
            }
        }
        // Bounded FIFO, owned by [`MonitorQueue`] (pvxs `doPost`): the
        // connect-time seed is `pending[0]` (the consumer emits it first at
        // START, ahead of the accrued backlog) rather than an unconditional
        // pre-loop send. The seed is pushed RAW — the consumer runs the
        // `_filter` chain on every pending item (so the seed is filtered
        // exactly once, like epics-base `dbChannelRunPreChain`; a gating filter
        // that drops it suppresses the initial frame, a transform mismatch
        // tears the monitor down with an error).
        let mut pending = MonitorQueue::new(queue_limit, &intro_clone, &mask_clone);
        if let Some(initial) = seed_initial {
            pending.seed(initial);
        }

        let mut source_open = true;
        loop {
            let executing = *exec_rx.borrow_and_update();
            // Apply this op's gate from the state we just read. The
            // `exec_rx.changed()` arm below wakes the loop on every edge, so
            // this reaches the source's suspend/resume as promptly as the
            // driver task did — including while the loop is parked waiting
            // for an update that a suspended upstream will never send.
            gate_driver.apply(executing).await;
            // The terminal FINISH is Executing-gated, exactly like every DATA
            // frame: pvxs holds both the backlog and the finish until the client
            // is Executing (servermon.cpp:82,142-154). Break — and so send the
            // post-loop FINISH — only once the source is closed AND we are
            // Executing AND the backlog has drained. An Idle or never-STARTed
            // monitor whose PV closes therefore accrues the finish instead of
            // emitting FINISH and abandoning `pending`; a later START flushes the
            // backlog then finishes. Teardown (DESTROY / disconnect) still aborts
            // the task via `monitor_abort` while it waits for that START.
            if !source_open && executing && pending.is_empty() {
                break;
            }
            // Arm the credit-refill waiter BEFORE reading the window (see
            // `arm_refill`: the ACK's `notify_waiters()` leaves no permit, so
            // registering after the read could lose the wake-up).
            let refill = credit.arm_refill();
            // pvxs `maybeReply`/`doReply`: an exhausted pipeline window
            // suppresses the REPLY only (`servermon.cpp:79-83,143`). It must
            // not suppress the DRAIN — `doPost` keeps squashing into the
            // negotiated queue while the client owes ACKs. So this is an emit
            // GATE, not an await inside the arm: `rx.recv()` stays polled and
            // a stalled pipelined client coalesces at `limit` instead of
            // making the port buffer `channel_capacity + limit` distinct
            // updates and deliver them all on resume.
            let has_credit = credit.available();
            tokio::select! {
                biased;
                r = rx.recv(), if source_open => {
                    match r {
                        Some(ev) => {
                            pending.push(ev);
                            while let Ok(e) = rx.try_recv() {
                                pending.push(e);
                            }
                        }
                        None => source_open = false,
                    }
                }
                _ = exec_rx.changed() => {}
                // Re-evaluate the gate when an ACK refills the window.
                _ = wait_credit_refill(refill), if !has_credit => {}
                _ = std::future::ready(()), if executing && has_credit && !pending.is_empty() => {
                    let mut value = pending.pop().expect("guarded non-empty");
                    // Subscription boundary (upstream descriptor change): emit
                    // MONITOR FINISH and end — the decoded counterpart of the raw
                    // path's `type_changed` branch.
                    if value.type_changed {
                        let finish = build_monitor_finish(ioid, order_now());
                        let _ = tx_clone.send(finish).await;
                        return;
                    }
                    // Per-event ACL recheck on policy reload, routed through
                    // `revalidate_read` for composite-source correctness.
                    let live_v = src.access_gate().acl_version();
                    if live_v
                        != mon_acl_version_at_subscribe_cell
                            .load(std::sync::atomic::Ordering::Acquire)
                    {
                        if src.revalidate_read(&pv_name, mon_ctx.clone()).await.is_none() {
                            let finish = build_monitor_finish(ioid, order_now());
                            let _ = tx_clone.send(finish).await;
                            return;
                        }
                        mon_acl_version_at_subscribe_cell
                            .store(live_v, std::sync::atomic::Ordering::Release);
                    }
                    // Outbound-queue depth: server diagnostic only.
                    let outbound_pending = tx_clone.max_capacity() - tx_clone.capacity();
                    if outbound_pending >= high_watermark && !queue_over_high {
                        queue_over_high = true;
                        warn!(
                            pv = %pv_name,
                            pending = outbound_pending,
                            high_watermark,
                            "monitor outbound queue crossed high watermark"
                        );
                    } else if outbound_pending == 0 && queue_over_high {
                        queue_over_high = false;
                        debug!(pv = %pv_name, "monitor outbound queue drained");
                    }
                    let marked = value.marked.take();
                    let value = value.value;
                    // Server-side channel filters: skip when the chain drops this
                    // event (no wire frame => no credit consumed).
                    let value = match apply_monitor_filter_chain(&filters, &value, &intro_clone) {
                        MonitorFilterOutcome::Pass => value,
                        MonitorFilterOutcome::Drop => continue,
                        MonitorFilterOutcome::Transformed(tv) => tv,
                        MonitorFilterOutcome::DescriptorMismatch => {
                            let err = build_monitor_error(
                                ioid,
                                "server-side filter transform does not fit the monitor descriptor",
                                order_now(),
                            );
                            let _ = tx_clone.send(err).await;
                            return;
                        }
                    };
                    // Pipeline window: consume one credit AFTER the pause/filter
                    // gates (pvxs `servermon.cpp:192`) — a held or filtered event
                    // produces no wire frame, so it must not consume a slot. The
                    // credit was checked by the arm's `has_credit` gate and this
                    // is the only decrementer, so it is still there. No-op for a
                    // non-pipeline monitor.
                    credit.take();
                    // A source that declares its marked leaves frames exactly
                    // those (pvxs `to_wire_valid(R, ent, &pvMask)`); one that
                    // declares none posts a wholly-changed value, which is
                    // pvxs's fully-marked `Value` — the full request mask.
                    // There is no third form: the port does not reconstruct a
                    // marked set by diffing snapshots, which pvxs never does.
                    let payload = build_monitor_payload(
                        ioid,
                        &intro_clone,
                        &value,
                        marked.as_deref(),
                        &mask_clone,
                        order_now(),
                    );
                    if tx_clone.send(payload).await.is_err() {
                        return;
                    }
                }
            }
        }
        // Source closed — emit MONITOR FINISH (pvxs servermon.cpp:148-178).
        let finish = build_monitor_finish(ioid, order_now());
        let _ = tx_clone.send(finish).await;
    });

    (join, start_ctl)
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
    /// PVA `ARRAY` (cmd 14): ChannelArray windowed-array operation
    /// (INIT binds an array field, then get/put a `[offset,count,stride]`
    /// slice or get/set its length). pvAccessCPP
    /// `responseHandlers.cpp:2115-2208`.
    Array,
    /// PVA `GET_FIELD` (cmd 17): one-shot introspection. pvxs models this
    /// as a real `ServerIntrospect` op in `opByIOID`
    /// (serverintrospect.cpp:141-178); the Rust slow path reserves its
    /// IOID in `ch.ops` under this kind while the source introspection is
    /// in flight.
    GetField,
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
            OpKind::Array => Command::Array,
            OpKind::GetField => Command::GetField,
        }
    }
}

// `ClientCredentials` moved to [`super::config`] with `PvaServerConfig`, whose
// `auth_complete` hook names it in its public signature — the config cannot be
// target-neutral while the type in its own signature is not. It is identity
// data, not socket code. Re-exported so `server_native::tcp::ClientCredentials`
// keeps resolving for every existing caller.
pub use super::config::ClientCredentials;

// The credential *record* is target-neutral and lives beside the config;
// building one from a CONNECTION_VALIDATION reply or a verified TLS chain is
// this module's job, so the constructors stay here.
impl ClientCredentials {
    /// The ACF host identity for a connection: its peer address, numeric.
    ///
    /// Byte-for-byte what QSRV does (`ioc/credentials.cpp:27-29`):
    ///
    /// ```cpp
    /// SockAddr addr(clientCredentials.peer);
    /// addr.setPort(0);
    /// host = std::string(SB()<<addr.map6to4());
    /// ```
    ///
    /// `setPort(0)` is why only the IP is taken, and `map6to4` is why an
    /// IPv4-mapped IPv6 peer renders as its IPv4 form — so a client reaching a
    /// dual-stack listener matches the same HAG entry it would over IPv4.
    /// [`Ipv6Addr::to_ipv4_mapped`](std::net::Ipv6Addr::to_ipv4_mapped) is the exact counterpart (`to_ipv4` is
    /// not: it also maps IPv4-*compatible* addresses, turning `::1` into
    /// `0.0.0.1`).
    ///
    /// Numeric, not reverse-DNS, and deliberately: a reverse lookup is a
    /// second network operation that can fail, and its failure path lands
    /// back in the sentinel/empty-string behaviour this whole change exists
    /// to remove. On a target with no resolver that failure is the *expected*
    /// branch, not the exceptional one. Numeric is also what upstream
    /// compares — QSRV never consults `asCheckClientIP`, so its PVA host is
    /// always the numeric peer.
    fn acf_host_from_peer(peer: std::net::SocketAddr) -> String {
        match peer.ip() {
            std::net::IpAddr::V4(v4) => v4.to_string(),
            std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => v4.to_string(),
                None => v6.to_string(),
            },
        }
    }

    /// Derive every server-side identity field from server-side truth:
    /// [`Self::roles`] from [`Self::account`] against the local passwd/group
    /// DB (pvxs `ClientCredentials::roles` → `osdGetRoles`), and
    /// [`Self::host`] from the connection's peer address (QSRV
    /// `ioc/credentials.cpp:27-29`).
    ///
    /// The single funnel every constructor and parse path passes through, so
    /// both fields are server-derived **by construction** and a
    /// wire-advertised value can never reach the ACF gate. `host` used to be
    /// copied verbatim off the CONNECTION_VALIDATION body, three lines above
    /// the comment explaining why `roles` must not be; taking the peer here —
    /// rather than overwriting the wire value afterwards — is what makes the
    /// wire value unable to reach the field at all, instead of merely
    /// corrected after the fact.
    fn with_server_derived(mut self, peer: std::net::SocketAddr) -> Self {
        self.roles = crate::auth::osd_get_roles(&self.account);
        self.host = Self::acf_host_from_peer(peer);
        self
    }

    fn anonymous(peer: std::net::SocketAddr) -> Self {
        Self {
            method: "anonymous".into(),
            account: "anonymous".into(),
            host: String::new(),
            authority: String::new(),
            roles: Vec::new(),
        }
        .with_server_derived(peer)
    }

    /// Build `x509` credentials from a verified TLS peer chain.
    /// Mirrors pvxs `SSLContext::fill_credentials`: the leaf cert's
    /// subject CommonName becomes the `account` and the root CA's
    /// subject CommonName becomes the `authority`.
    fn x509(creds: crate::auth::X509Credentials, peer: std::net::SocketAddr) -> Self {
        Self {
            method: "x509".into(),
            account: creds.account,
            host: String::new(),
            authority: creds.authority,
            roles: Vec::new(),
        }
        .with_server_derived(peer)
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
/// pvxs `serverconn.cpp:204-216` always decodes the auth
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
/// Auth methods this server advertises in CONNECTION_VALIDATION
/// (pvxs serverconn.cpp:103-114 — exactly "anonymous"/"ca"). Any other
/// spelling, including case variants like "CA", is treated as an
/// unadvertised method and rejected (serverconn.cpp:238-241).
const ADVERTISED_AUTH_METHODS: &[&str] = &["anonymous", "ca"];

/// Process a single CONNECTION_VALIDATION frame: parse the client's auth
/// payload, commit the resulting credential into `cred`, reply
/// CONNECTION_VALIDATED, and fire the `auth_complete` hook.
///
/// pvxs routes CONNECTION_VALIDATION through one `handle_CONNECTION_VALIDATION`
/// path on every dispatch — both the initial handshake and any later
/// re-auth ("Client begins (restarts?) Auth handshake",
/// serverconn.cpp:196-251). Running the full parse → commit →
/// validated-reply sequence on each frame is what lets a post-handshake
/// re-auth update the connection identity and emit a fresh
/// CONNECTION_VALIDATED, so this helper is the single owner of that
/// transition and is invoked from both the pre-handshake branch and the
/// application dispatcher.
#[allow(clippy::too_many_arguments)]
async fn process_connection_validation(
    frame: &Frame,
    tx: &SrvTx,
    order: ByteOrder,
    x509_locked: bool,
    cred: &mut ClientCredentials,
    peer: SocketAddr,
    peer_entry: &crate::server_native::peers::PeerEntry,
    config: &PvaServerConfig,
    decode_cache: &mut TypeCache,
) -> PvaResult<()> {
    // Parse the client's auth payload: skip buffer_size (u32),
    // introspection_size (u16), qos (u16); read selected method
    // (string); when method == "ca", read the type+value of the
    // auth Value and pull out the `user` / `host` fields. Pure
    // metadata for audit/logging.
    // when the connection is mTLS-authenticated, the
    // x509 identity from the verified cert chain wins — the
    // client's CONNECTION_VALIDATION claim is parsed only
    // for diagnostics and never replaces it.
    //
    // `advertised` records whether the *effective* method (mTLS x509, or the
    // plain-TCP claim) is one this server advertised. It is the only gate on
    // the OK-vs-Error reply below, and `*cred` is committed only on the
    // advertised path — so a rejected re-auth never mutates the connection
    // identity.
    let advertised;
    if x509_locked {
        // a decode fault here is still fatal —
        // log + propagate. Pre-fix swallowed; pvxs
        // `serverconn.cpp:211-216` calls `bev.reset()`.
        match parse_client_credentials(frame, decode_cache, peer)? {
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
        // mTLS: the verified x509 chain is the identity and is always an
        // advertised method (pvxs advertises `x509` for TLS transports); the
        // client's CONNECTION_VALIDATION claim never replaces `cred`.
        advertised = true;
    } else {
        // Parse the client's claim into a CANDIDATE without committing it.
        // pvxs clones the current `cred` into a local `C` (serverconn.cpp:221)
        // and commits `cred = C` only via an advertised method, so a rejected
        // unadvertised re-auth leaves the previous identity in force. Mirror
        // that: decode into `candidate`, decide whether the effective method is
        // advertised, and write `*cred` ONLY on the advertised path. A decode
        // fault is connection-fatal (pvxs serverconn.cpp:211-216 bev.reset). An
        // anonymous handshake (empty/"anonymous" method) returns Ok(None) and
        // keeps whatever credential is already in force — anonymous on a fresh
        // connection, the committed identity on a re-auth.
        let candidate = parse_client_credentials(frame, decode_cache, peer)?;
        // The method that would take effect: the claim's method when the client
        // sent one, otherwise the credential already in force (so an anonymous
        // re-handshake stays advertised against the live method, never resetting
        // a committed `ca` identity).
        let effective_method = candidate
            .as_ref()
            .map_or(cred.method.as_str(), |c| c.method.as_str());
        advertised = ADVERTISED_AUTH_METHODS.contains(&effective_method);
        if !advertised {
            // Unadvertised method: `*cred` is left untouched. pvxs treats a
            // rejected re-auth as a rejected credential *update*, not a logout
            // (serverconn.cpp:221-241: `C` started as a copy of the old cred and
            // no advertised method changed it), so the previous identity —
            // alice/ca on a re-auth, anonymous on a fresh connection — stays the
            // connection's effective credential. The Status::Error reply below
            // tells the client its elevated claim was rejected.
            debug!(
                ?peer,
                rejected_method = %effective_method,
                kept_method = %cred.method,
                kept_account = %cred.account,
                "PVA client selects unadvertised auth method — replying \
                 Status::Error, keeping the previous credential"
            );
        }
        if advertised {
            // Advertised method: commit the claim. A `None` candidate
            // (anonymous handshake) keeps the current credential unchanged.
            if let Some(claimed) = candidate {
                *cred = claimed;
            }
        }
    }
    debug!(?peer, method = %cred.method, account = %cred.account,
        authority = %cred.authority, roles = ?cred.roles,
        "PVA client credentials");
    // pvxs `serverconn.cpp:238-241` parity: when the client picks an auth
    // method we never advertised, reply CONNECTION_VALIDATED with
    // Status::Error so the client knows its elevated identity claim was
    // rejected. The connection stays open and the effective credential is
    // whatever was already in force — never the rejected claim, and never a
    // forced downgrade to anonymous. Matches "No practical
    // way to handle auth failure. So we accept all credentials, but may not
    // grant rights." `advertised` was decided above from the effective method
    // (mTLS x509 is always advertised) and `*cred` was committed only on the
    // advertised path, so there is nothing to revert here.
    let validated_status = if advertised {
        Status::ok()
    } else {
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
    // Commit the validated credential BEFORE the
    // CONNECTION_VALIDATED frame leaves. pvxs finalises
    // `cred` (serverconn.cpp:234) before `auth_complete()`
    // enqueues CONNECTION_VALIDATED (serverconn.cpp:191), so
    // any observer that has seen VALIDATED must already see
    // the committed identity. Record it for the per-peer
    // report and fire the user-installed `auth_complete`
    // hook (pvxs serverconn.cpp:181 parity — peer addr +
    // credentials snapshot; ACF integration goes here)
    // first, then send. Sending first left a window where a
    // client that observed VALIDATED could race ahead of the
    // hook (the cause of the flaky auth-hook regression).
    peer_entry.set_credentials(&cred.account, &cred.method);
    if let Some(hook) = config.auth_complete.as_ref() {
        hook(peer, cred);
    }
    let _ = tx.send(buf).await;
    Ok(())
}

/// `peer` is taken because the ACF host identity is derived from it, never
/// from the body being parsed — see `with_server_derived`.
fn parse_client_credentials(
    frame: &Frame,
    decode_cache: &mut TypeCache,
    peer: std::net::SocketAddr,
) -> PvaResult<Option<ClientCredentials>> {
    // Inbound application payloads are decoded with the frame's own header
    // byte order (pvxs latches `peerBE` per received message,
    // conn.cpp:195-198), never the server's configured outbound order.
    let order = frame.order();
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
    // pvxs serverconn.cpp:223-231 — method/account are set from
    // the auth body ONLY for selected=="ca" with a valid user field;
    // every other path lands on method="anonymous"/account="anonymous"
    // via `if(C->method.empty())`. Mirror that: an empty method or an
    // explicit "anonymous" selection both yield Ok(None), letting the
    // caller keep the pre-initialised ClientCredentials::anonymous()
    // (account="anonymous"). pvxs clients always send the string
    // "anonymous" — not empty — so the is_empty() guard alone left
    // account="" for every pvxs anonymous handshake.
    //
    // Byte-exact match: pvxs advertises exactly "anonymous"/"ca"
    // (serverconn.cpp:108-114) and compares the selected method as a raw
    // string (serverconn.cpp:202-231). A non-exact spelling like
    // "Anonymous" is NOT the advertised method — it falls through and is
    // rejected as unadvertised by the caller (serverconn.cpp:238-241),
    // rather than being case-folded into a clean anonymous handshake.
    if method.is_empty() || method == "anonymous" {
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
    // A leading `0xFF` is the pvxs "null type" tag: the auth Value carries
    // no `user`/`host` fields. pvxs `serverconn.cpp:223-231` only sets
    // method/account inside the `auth["user"]` callback, so a null body
    // leaves the placeholder `anonymous/anonymous` exactly as a structure
    // with no `user` field does. Rather than return early here — which left
    // a null-auth "ca" handshake as `method="ca", account=""` and skipped
    // the shared ca-requires-user rule below — fall through with an empty
    // account so both the null and empty-structure paths take one rule.
    //
    // Routed through the connection-scope decode cache so a client that
    // advertises its auth structure with a `0xFD <slot>` define here can
    // later reference it by `0xFE <slot>` from a pvRequest/EXEC body, and
    // vice versa — pvxs shares one connection `rxRegistry` (conn.h:23)
    // across every inbound decode, including CONNECTION_VALIDATION.
    let auth_desc =
        crate::pvdata::encode::decode_type_desc_cached_opt(&mut cur, order, decode_cache)
            .map_err(|e| PvaError::Decode(format!("CONN_VALIDATION auth desc: {e}")))?;
    if let Some(desc) = auth_desc {
        let value = decode_pv_field_cached(&desc, &mut cur, order, decode_cache)
            .map_err(|e| PvaError::Decode(format!("CONN_VALIDATION auth value: {e}")))?;
        if let PvField::Structure(s) = value {
            for (name, field) in &s.fields {
                match (name.as_str(), field) {
                    ("user", PvField::Scalar(crate::pvdata::ScalarValue::String(v))) => {
                        creds.account = v.as_str_lossy().into_owned();
                    }
                    // NOTE the absence of a `host` arm, and do not add one.
                    // A client MAY advertise `host`, and pvxs has no field to
                    // put it in: `server::ClientCredentials`
                    // (`src/pvxs/srvcommon.h:36-56`) carries peer, iface,
                    // method, account, raw and roles() and no host at all.
                    // QSRV derives the ACF host from the socket instead
                    // (`ioc/credentials.cpp:27-29`). We used to copy the wire
                    // string straight into the field the HAG gate matches,
                    // which let a client pick its own host identity — so the
                    // field is now written only by `with_server_derived`, from
                    // the peer, and this parser cannot reach it.
                    //
                    // Same rule, same reason, as the `groups`/`roles` field a
                    // client MAY also advertise: trusting it would be an ACL
                    // bypass. Both are ignored here and derived below.
                    _ => {}
                }
            }
        }
    }
    // pvxs serverconn.cpp:223-231 — for "ca", the credential is
    // only meaningful when a user field was present (the lambda sets
    // BOTH method and account). Without a user field the lambda never
    // fires, C->method stays empty, and the anonymous fallback triggers.
    // Mirror that: ca with a missing or empty user field (including the
    // null-auth body above) returns Ok(None) so the caller falls back to
    // ClientCredentials::anonymous().
    //
    // Byte-exact "ca" (pvxs serverconn.cpp:221-231 keys on selected=="ca"):
    // a non-exact spelling like "CA" is not "ca", so this fallback does not
    // fire; it returns the claimed credential and the caller rejects it as
    // an unadvertised method, leaving the previous connection identity in
    // force (the claim is never committed; see process_connection_validation).
    if creds.method == "ca" && creds.account.is_empty() {
        return Ok(None);
    }
    // Roles are re-derived server-side from `account` (pvxs
    // `ClientCredentials::roles()`) and the host from the peer socket (QSRV
    // `ioc/credentials.cpp:27-29`); any wire-advertised `groups`/`roles`/`host`
    // was ignored above.
    Ok(Some(creds.with_server_derived(peer)))
}

/// Type-erased read/write halves so the same handler works for plain TCP
/// and TLS-wrapped streams.
type SrvRead = Box<dyn tokio::io::AsyncRead + Unpin + Send>;
type SrvWrite = Box<dyn tokio::io::AsyncWrite + Unpin + Send>;
/// Per-connection write side. Producers (the main read loop — including
/// its heartbeat arm — and the monitor subscribers) push fully-framed
/// PVA messages into the
/// channel; a single dedicated writer task drains it in arrival order.
/// Replaces `Arc<Mutex<SrvWrite>>` so a slow client cannot block other
/// producers waiting for the lock. The channel is *bounded* —
/// `await`-style sends propagate backpressure all the way back to the
/// monitor subscribers / read loop, so memory cannot grow unbounded
/// when the client is slow. Errors on the write side drop the
/// receiver; subsequent sends fail and the read loop independently
/// observes the dead socket and tears down.
type SrvTx = tokio::sync::mpsc::Sender<Vec<u8>>;

/// Connection writer handle bound to one channel's byte accounting.
///
/// pvxs increments `chan->statTx` by the encoded length at *every*
/// per-channel send site (`serverget.cpp:124`, `servermon.cpp:186`,
/// `serverintrospect.cpp:45`, `serverchan.cpp:151-152`). The writer
/// task here owns only an opaque `Vec<u8>` stream and cannot recover
/// the owning channel, so the count has to be taken where the channel
/// is known. Threading a raw `SrvTx` plus a manual `stat.add_tx(..)`
/// before each `send` re-opens the exact defect this finding names —
/// a missed site silently under-counts. `ChannelTx` is the single
/// owner of that accounting: holding one is the only way an op task
/// can emit a per-channel frame, so the count cannot be skipped by
/// construction. Connection-level frames (heartbeat, SET_BYTE_ORDER,
/// validation, pre-resolution errors) keep the raw `SrvTx` — they
/// belong to no channel.
#[derive(Clone)]
struct ChannelTx {
    tx: SrvTx,
    stat: Arc<crate::server_native::peers::ChannelStat>,
}

impl ChannelTx {
    fn new(tx: SrvTx, stat: Arc<crate::server_native::peers::ChannelStat>) -> Self {
        Self { tx, stat }
    }

    /// Emit a per-channel frame, charging its length to the channel's
    /// `statTx` first so the report stays exact even if the send races
    /// a writer-task shutdown.
    async fn send(&self, buf: Vec<u8>) -> Result<(), tokio::sync::mpsc::error::SendError<Vec<u8>>> {
        self.stat.add_tx(buf.len());
        self.tx.send(buf).await
    }

    /// Current free capacity of the underlying writer channel — used by
    /// the monitor outbound-queue-depth diagnostic. Delegates to the
    /// wrapped `Sender`; carries no byte accounting.
    fn capacity(&self) -> usize {
        self.tx.capacity()
    }

    /// Total capacity of the underlying writer channel (see
    /// [`Self::capacity`]).
    fn max_capacity(&self) -> usize {
        self.tx.max_capacity()
    }
}

/// result of a spawned CREATE_CHANNEL resolver task. The read
/// loop's `channels` HashMap is owned by the loop task; spawned
/// resolver tasks cannot touch it directly. Instead they send this
/// completion record through a dedicated mpsc, and the read loop's
/// `select!` arm applies the insertion and emits the wire response in
/// frame-arrival order (mpsc is FIFO).
struct CreateChannelCompletion {
    cid: u32,
    sid: u32,
    name: String,
    /// Credential in force when CREATE_CHANNEL was dispatched, captured
    /// before the async resolver runs and carried back so the channel's
    /// lifecycle callbacks use the credential the channel was opened under —
    /// not whatever the connection re-authenticated to while the resolver
    /// was still running (pvxs `serverchan.cpp:62`).
    open_cred: ClientCredentials,
    /// `Some` → PV was found; carries the negotiated descriptor and the
    /// owner source bound into the channel. `None` → not found; emit an
    /// error response and insert no channel. Folding "found" and "owner"
    /// into one optional makes the invariant "a found channel always has
    /// a bound owner" hold by construction — there is no `found == true`
    /// state without an owner to bind.
    resolved: Option<ResolvedChannel>,
}

/// Successful CREATE_CHANNEL resolution: the descriptor negotiated for
/// the channel plus the owner source that accepted it.
struct ResolvedChannel {
    intro: Option<Arc<FieldDesc>>,
    owner: DynSource,
    /// Source-supplied contextual info for the report's per-channel
    /// `info` field, queried from the bound owner at resolution time —
    /// pvxs `ServerChannelControl::updateInfo` (`source.h:192`) stashed
    /// into `chan->reportInfo`. `None` when the source attaches nothing.
    report_info: Option<String>,
}
/// Sender half of the CREATE_CHANNEL completion channel.
type CcTx = mpsc::Sender<CreateChannelCompletion>;

/// Per-connection context the accept loop establishes before handing the
/// split stream to [`handle_connection_io`]: the peer's report entry, the
/// verified mTLS identity (if any), and the server-wide channel-invalidation
/// sender. Bundled to keep the IO handler's argument count within budget.
pub(super) struct ConnInit {
    pub(super) peer_entry: Arc<crate::server_native::peers::PeerEntry>,
    /// x509 identity from the verified TLS peer chain, when this connection
    /// arrived over mutually-authenticated TLS. `None` for plain TCP or TLS
    /// without a client cert. When present it is the authoritative identity
    /// and overrides the CONNECTION_VALIDATION claim — mirrors pvxs
    /// `SSLContext::fill_credentials`.
    pub(super) x509_identity: Option<crate::auth::X509Credentials>,
    /// Server-wide channel invalidator (see
    /// [`ChannelSource::set_channel_invalidator`](crate::server_native::ChannelSource::set_channel_invalidator)). Subscribed once below; a
    /// published PV name force-disconnects every channel this connection
    /// currently serves under that name with a server-initiated
    /// DESTROY_CHANNEL.
    pub(super) channel_invalidator: ChannelInvalidator,
}

pub(super) async fn handle_connection_io(
    source: DynSource,
    mut reader: SrvRead,
    mut writer_raw: SrvWrite,
    peer: SocketAddr,
    config: PvaServerConfig,
    init: ConnInit,
) -> PvaResult<()> {
    let ConnInit {
        peer_entry,
        x509_identity,
        channel_invalidator,
    } = init;
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
    //    We wrap `write_all` in `runtime::task::timeout(send_timeout)`
    //    so a stalled write breaks the task, closes the mpsc, and
    //    fails fast. Mirrors the parallel guard in `epics-ca-rs`'s
    //    server-side dispatch wrap (the CA G1 audit fix).
    let send_tmo = config.send_timeout;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(config.write_queue_depth);
    let writer_peer = peer;
    let peer_entry_writer = peer_entry.clone();
    let writer_task = epics_base_rs::runtime::task::spawn(async move {
        while let Some(frame) = rx.recv().await {
            match epics_base_rs::runtime::task::timeout(send_tmo, writer_raw.write_all(&frame))
                .await
            {
                Ok(Ok(())) => {
                    // bytes_out counter for PvaServer::report().
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
    // abort the writer task the moment the read loop returns.
    // Without this it lingers up to `idle_timeout` (default 45s) holding
    // the writer half of the (now-disconnected) socket.
    // pvxs uses libevent-driven cleanup that shuts everything in one
    // pass; we rely on the spawn seam's handle `abort()` via AbortOnDrop.
    // The heartbeat needs no such guard: it is a deadline arm of this
    // loop's `select!` (see `hb_tick` below), so it ends with the loop.
    let _writer_guard = AbortOnDrop(writer_task.abort_handle());

    // Per-connection liveness for the idle-timeout watchdog. A plain local,
    // not an `Arc<AtomicU64>`: the read loop both stamps it (on every frame)
    // and reads it (in the heartbeat arm), so there is no second owner to
    // share it with.
    let mut last_rx = now_nanos();

    // Server-side echo heartbeat as a deadline arm of the read loop rather
    // than a per-connection task: send ECHO_REQUEST every 15 s, and stop
    // beating once the peer has been silent for `idle_timeout`.
    //
    // `Interval::tick` is cancel-safe — losing the race in `select!` consumes
    // no tick — so holding the interval across iterations reproduces the
    // task's fixed 15 s cadence exactly, including tokio's default `Burst`
    // catch-up if a long dispatch delays a tick.
    let mut hb_tick = epics_base_rs::runtime::task::interval(Duration::from_secs(15));
    // `interval` yields its first tick immediately; the task consumed it the
    // same way, so the first beat lands 15 s in.
    hb_tick.tick().await;
    // Latched when the idle watchdog fires or the writer channel closes —
    // the two conditions that used to `break` the task's loop. Note this
    // stops the *heartbeat*, not the connection: the task ending never tore
    // the connection down either (its `JoinHandle` was only ever aborted),
    // so the existing "closing" wording overstates what happens.
    let mut hb_stopped = false;

    // Outbound byte order as a shared, mutable per-connection cell, seeded
    // from the configured wire order. The read loop latches a new value if
    // the peer sends a SET_BYTE_ORDER control frame mid-stream (pvxs
    // conn.cpp:169-188 `sendBE`; old pvAccess accepts it from either peer
    // at any time). `true` = Big. Single writer (the read loop, which also
    // keeps an in-sync local `order` for the synchronous dispatch path it
    // owns); the readers are the spawned MONITOR subscriber tasks, which
    // stamp each frame with the current order via their `order_now()`.
    let out_order = Arc::new(std::sync::atomic::AtomicBool::new(
        config.wire_byte_order.is_big(),
    ));

    // Outbound order owner: this read loop. Mutable so a mid-stream
    // SET_BYTE_ORDER from the peer re-latches it (pvxs conn.cpp:169-188);
    // every synchronous handler call below passes the current value and
    // the shared `out_order` cell mirrors it for the heartbeat task.
    let mut order = config.wire_byte_order;

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
    // pvxs `serverconn.cpp:108-114` writes "anonymous" first,
    // then "ca", with a comment explaining that older pvAccess
    // clients took the LAST known plugin on the wire. The reverse-
    // priority order matters: an old client picks the last
    // recognised method as its preferred. Pre-fix Rust sent
    // `["ca", "anonymous"]` which made such old clients pick
    // anonymous and silently drop user/host credentials — changing
    // ACF decisions even though the comment claimed pvxs parity.
    // Modern pvxs clients explicitly prefer `ca`; validation still
    // accepts both, only the wire order changes.
    // match pvxs serverconn.cpp:103-104 — serverReceiveBufferSize = 0x10000 ("not used").
    let val_req =
        build_server_connection_validation(order, 0x10000, 32_767, ADVERTISED_AUTH_METHODS);
    let _ = tx.send(val_req).await;

    // Step 3+: drive the read loop.
    let mut rx_buf: Vec<u8> = Vec::with_capacity(8192);
    let mut channels: HashMap<u32, ChannelState> = HashMap::new();
    // Receiver on the server-wide channel invalidator. A source publishes a
    // batch of PV names that must be force-disconnected out of band (PVA
    // gateway operator `:drop`/`:flush`); the read-loop arm below
    // force-disconnects every channel in `channels` serving any of those
    // names with a server-initiated DESTROY_CHANNEL. The queue is unbounded
    // and per-connection, so a large `:flush` can never drop a name on this
    // connection. Subscribing here (post-accept,
    // pre-handshake) is sufficient: no channel exists until after
    // CONNECTION_VALIDATION, so any invalidation during the handshake names
    // a PV this connection cannot yet be serving and is harmlessly missed.
    let mut inv_rx = channel_invalidator.subscribe();
    // Disables the invalidation select! arm once every sender has dropped
    // (server shutdown). Without the guard a closed mpsc `recv()` resolves
    // `None` immediately and forever, busy-looping the select.
    let mut inv_closed = false;
    let mut handshake_complete = false;
    // Client identity carried for the rest of the connection lifetime.
    //
    // Precedence (mirrors pvxs):
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
        Some(id) => ClientCredentials::x509(id, peer),
        None => ClientCredentials::anonymous(peer),
    };
    // Per-connection emit-side TypeStore. Only consulted when
    // `config.emit_type_cache` is true (off by default for pvAccessCPP
    // compatibility — that client does not parse 0xFD/0xFE markers).
    let mut encode_type_cache = crate::pvdata::encode::EncodeTypeCache::new();
    // Per-connection inbound (decode-side) TypeStore — the receiver mirror
    // of `encode_type_cache`. pvxs keeps one connection-scoped `rxRegistry`
    // (conn.h:23) shared by every inbound decode; a client may define a
    // descriptor with `0xFD <slot> <desc>` in one frame (auth body,
    // pvRequest INIT, RPC/PUT EXEC value) and reference it with `0xFE <slot>`
    // in any later frame on the same connection. The read loop dispatches
    // every frame synchronously in wire order, so a define is always folded
    // into this cache before a later reference resolves against it — the
    // read loop is the single owner of inbound type-cache state, exactly as
    // the connection reader task owns it client-side.
    let mut rx_type_cache = TypeCache::new();

    let max_msg_size = config.max_message_size;
    // segmented-message reassembly state. pvxs conn.cpp:228-291
    // accumulates SegFirst..SegMiddle..SegLast bodies into `segBuf`
    // before dispatching. Without this, our server would treat every
    // segment as a fresh message, decode garbage, and likely return
    // a Decode error mid-payload. Sites that put bulk values
    // (NTTable, large NTNDArray, multi-MiB NTScalarArray) over PVA
    // hit segmented frames whenever the message exceeds the peer's
    // buffer-size hint negotiated in CONNECTION_VALIDATION.
    let mut seg_buf: Vec<u8> = Vec::new();
    let mut seg_cmd: u8 = 0;
    // Byte order of the message's initiating (SegFirst) frame. The
    // reassembled synthetic frame must carry this order, not the server's
    // configured outbound order, so downstream handlers decode the payload
    // with the peer's actual order (pvxs latches `peerBE` once per logical
    // message, conn.cpp:195-198). Seeded with `order` but always overwritten
    // on the first segment before any synthetic frame is built.
    let mut seg_order = order;
    let mut expect_seg = false;
    // CREATE_CHANNEL completion channel. Spawned resolver
    // tasks send results here; the read loop's select! arm applies
    // insertions into `channels` and emits wire responses in arrival
    // order (mpsc FIFO preserves the per-frame ordering guarantee).
    let (cc_tx, mut cc_rx) = mpsc::channel::<CreateChannelCompletion>(64);
    // MONITOR subscriber-completion channel. A spawned subscriber
    // task that ends (source close, descriptor change, ACL deny, filter
    // mismatch, raw re-encode terminal, panic, abort) signals its
    // `(sid, ioid, op_id)` here via `MonitorFinishGuard`; the select! arm
    // below removes the op through the owner, running the same
    // start-control / abort finalizers `DESTROY_REQUEST` runs. Unbounded
    // so the guard's sync `Drop` never loses a signal; the queue is
    // bounded in practice by the live op count, which `max_ops_per_channel`
    // already caps.
    let (mon_fin_tx, mut mon_fin_rx) = mpsc::unbounded_channel::<MonitorFinished>();
    // a spawned GET/PUT/RPC/PUT_GET/PROCESS data-phase task signals
    // here when its response is sent so the owner can return the op to `Idle`
    // (see [`ExecFinished`]/[`apply_exec_finish`]). Same unbounded-so-Drop-
    // never-loses rationale as `mon_fin_tx`.
    let (exec_fin_tx, mut exec_fin_rx) = mpsc::unbounded_channel::<ExecFinished>();
    // Count of in-flight CREATE_CHANNEL resolver tasks. Used in the
    // per-connection channel cap check: channels being resolved count
    // against the limit to prevent a burst of concurrent requests from
    // racing past it before the first completions arrive.
    let mut pending_channel_spawns: usize = 0;
    // Drive the read loop inside a block so EVERY exit path funnels
    // through the channel-close fan-out below: the writer-died
    // `return Ok(())`, any `?`-propagated decode/IO error, and the
    // idle/EOF teardown. pvxs runs `ServerChan::cleanup` for each
    // channel as the owning `ServerConn` is destroyed
    // (serverconn.cpp), so a per-channel close hook must fire on
    // connection teardown too, not only on explicit DESTROY_CHANNEL.
    let conn_result: PvaResult<()> = async {
        loop {
        // if the writer task has died (send_timeout fired,
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
        // select! between CREATE_CHANNEL completions (from
        // spawned resolver tasks) and new frames from the socket.
        // Servicing completions here rather than inline in the
        // CREATE_CHANNEL handler lets the read loop stay unblocked
        // while has_pv() / get_introspection() run in the background.
        let frame = tokio::select! {
            cc_opt = cc_rx.recv() => {
                // A spawned CREATE_CHANNEL resolver finished.
                if let Some(cc) = cc_opt {
                    pending_channel_spawns = pending_channel_spawns.saturating_sub(1);
                    let mut payload = Vec::new();
                    payload.put_u32(cc.cid, order);
                    // On success the CREATE_CHANNEL reply is charged to the
                    // newly-created channel (pvxs serverchan.cpp:151-152
                    // `ch->statTx += 16u`); the failure reply belongs to no
                    // channel and stays connection-level.
                    let mut reply_stat: Option<Arc<crate::server_native::peers::ChannelStat>> = None;
                    if let Some(resolved) = cc.resolved {
                        payload.put_u32(cc.sid, order);
                        Status::ok().write_into(order, &mut payload);
                        // One shared per-channel report counter, held by both
                        // the connection's channel table and the PeerEntry
                        // (keyed by SID) so handler-side tx/rx attribution is
                        // visible to the report (pvxs chan->statTx/statRx).
                        let stat = crate::server_native::peers::ChannelStat::new(cc.name.clone());
                        // Attach the source-supplied report info captured at
                        // resolution — the single writer of the channel's
                        // `report_info`, surfaced as `Report::Channel::info`
                        // (pvxs copies `chan->reportInfo` into the report at
                        // `server.cpp`).
                        stat.set_report_info(resolved.report_info);
                        reply_stat = Some(stat.clone());
                        channels.insert(cc.sid, ChannelState {
                            name: cc.name,
                            cid: cc.cid,
                            sid: cc.sid,
                            introspection: resolved.intro,
                            source: resolved.owner,
                            stat: stat.clone(),
                            open_cred: cc.open_cred,
                            ops: HashMap::new(),
                        });
                        // Register the channel (live + lifetime counts and
                        // the per-channel report entry) in one owner call.
                        peer_entry.channel_opened(cc.sid, stat);
                        // Notify the bound source that a channel attached,
                        // matching pvxs `SharedPV::attach` running
                        // `onFirstConnect` on the empty→non-empty edge
                        // (sharedpv.cpp:299-313). This is a CHANNEL edge,
                        // independent of monitor subscription, so a
                        // GET/PUT/RPC/GET_FIELD-only client drives lazy open
                        // too. Paired with `close_channel`'s onClose.
                        if let Some(ch) = channels.get(&cc.sid) {
                            // Pinned to the channel's CREATE-time credential, not
                            // the connection's current `cred` — a re-auth between
                            // CREATE dispatch and this completion must not change
                            // which identity the source sees the channel open under.
                            let ctx = channel_lifecycle_ctx(peer, &ch.open_cred);
                            ch.source.notify_channel_open(&ch.name, &ctx);
                        }
                        // The attach hook above can lazily open a SharedPV that
                        // was still closed when the resolver snapshotted its
                        // descriptor (`resolved.intro == None`), e.g.
                        // `on_first_connect(|p| p.open(...))` (pvxs
                        // `sharedpv.cpp:299-313` runs `onFirstConnect` on the
                        // empty->non-empty channel edge). pvxs serves later
                        // operations from the owner's post-open descriptor; bind
                        // the owner, drive its open hook, THEN obtain and cache
                        // the descriptor from that SAME owner — so a GET / PUT /
                        // MONITOR INIT reads a real prototype instead of replying
                        // "must provide prototype" against a PV the hook just
                        // opened. Only fires when the snapshot was absent; an
                        // already-resolved descriptor (the common case) is left
                        // untouched, so this adds no source round-trip for a PV
                        // that was open at resolve time.
                        let refresh = channels.get(&cc.sid).and_then(|ch| {
                            ch.introspection.is_none().then(|| {
                                (
                                    ch.source.clone(),
                                    ch.name.clone(),
                                    channel_lifecycle_ctx(peer, &ch.open_cred),
                                )
                            })
                        });
                        if let Some((owner, name, ctx)) = refresh
                            && let Some(intro) = owner.get_introspection_checked(&name, ctx).await
                            && let Some(ch) = channels.get_mut(&cc.sid)
                        {
                            ch.introspection = Some(Arc::new(intro));
                        }
                    } else {
                        // CREATE_CHANNEL failure sid must be the
                        // no-channel sentinel 0xFFFFFFFF (pvxs
                        // serverchan.cpp:349, sid=-1), not 0.
                        payload.put_u32(CREATE_CHANNEL_NO_SID, order);
                        // An unclaimed channel is a *refused* channel, which
                        // pvxs reports as Fatal — not a recoverable Error —
                        // with the fixed message "Refused to create Channel"
                        // and the refusal trace "pvx:serv:refusechan:"
                        // (serverchan.cpp:328-351). Matching the status kind
                        // and trace lets conformance clients distinguish a
                        // refused channel from a recoverable operation error,
                        // and keeps the wire message PV-name-free like pvxs.
                        Status::Detailed {
                            kind: crate::proto::status::StatusKind::Fatal,
                            message: "Refused to create Channel".to_string(),
                            stack: "pvx:serv:refusechan:".to_string(),
                        }
                        .write_into(order, &mut payload);
                    }
                    let h = PvaHeader::application(
                        true, order,
                        Command::CreateChannel.code(),
                        payload.len() as u32,
                    );
                    let mut buf = Vec::new();
                    h.write_into(&mut buf);
                    buf.extend_from_slice(&payload);
                    if let Some(stat) = &reply_stat {
                        stat.add_tx(buf.len());
                    }
                    let _ = tx.send(buf).await;
                }
                continue;
            }
            fin_opt = mon_fin_rx.recv() => {
                // a MONITOR subscriber task ended. Remove its op
                // through the owner — dropping the `OpState` fires
                // `monitor_start_ctl` (terminal `notify_monitor_start(false)`)
                // and `monitor_abort` (already-ended task), identical to
                // DESTROY_REQUEST — gated on the op-instance id so a stale
                // signal cannot evict a re-INIT'd op that reused the ioid.
                //
                // Ordering: the guard enqueues this signal the instant the
                // task body's scope ends — right after the FINISH frame is
                // handed to the writer mpsc, strictly before the client can
                // receive that FINISH and send a fresh INIT. So the op is
                // already removed by the time any legitimate re-INIT of the
                // same ioid is read, and that re-INIT is accepted as fresh
                // rather than rejected on the duplicate-INIT fatal path.
                if let Some(fin) = fin_opt {
                    apply_monitor_finish(&mut channels, fin);
                }
                continue;
            }
            exec_opt = exec_fin_rx.recv() => {
                // a GET/PUT/RPC/PUT_GET/PROCESS data-phase task sent
                // its response and ended. Return the op to `Idle` through the
                // owner so a later explicit re-EXEC is accepted, gated on the
                // op-instance id so a stale signal cannot flip a re-INIT'd op
                // (a `lastRequest` op was already removed and is a no-op here).
                if let Some(fin) = exec_opt {
                    apply_exec_finish(&mut channels, fin);
                }
                continue;
            }
            inv_res = inv_rx.recv(), if !inv_closed => {
                // A source invalidated one or more channels out of band (PVA
                // gateway operator `<prefix>:drop` / `:flush`). Force-disconnect
                // every channel this connection currently serves under each
                // published PV name with a server-initiated DESTROY_CHANNEL —
                // the downstream effect of pva2pva dropping a
                // `ChannelCacheEntry`: `channel->destroy()` →
                // `channelStateChange(DESTROYED)` fanout to every interested
                // `GWChannel` (chancache.cpp:34-99, server.cpp:130-135).
                match inv_res {
                    Some(batch) => {
                        // One removal command publishes its whole removed set
                        // as a single unbounded-queue batch, so nothing is ever
                        // dropped, regardless of how many entries a `:flush`
                        // cleared. Tear down each name
                        // through the single teardown owner: a channel hosts
                        // every op under one name, so destroying it ends that
                        // name's GET/PUT/MONITOR alike — matching pva2pva's
                        // per-channel, not per-pvRequest, destroy granularity.
                        let teardown = ChannelTeardownCtx {
                            tx: &tx,
                            order,
                            peer,
                            peer_entry: &peer_entry,
                        };
                        for pv in batch.iter() {
                            invalidate_named_channels(pv, &mut channels, &teardown).await;
                        }
                    }
                    None => {
                        // Every sender dropped (server shutting down). Stop
                        // polling this arm so it cannot busy-loop; this
                        // connection keeps serving until its own teardown.
                        inv_closed = true;
                    }
                }
                continue;
            }
            _ = hb_tick.tick(), if !hb_stopped => {
                // Server-side echo heartbeat, formerly its own per-connection
                // task. Same cadence, same frame, same two stop conditions —
                // only the owner changed, from a task reading `last_rx` and
                // `out_order` through shared cells to this loop reading its
                // own `last_rx` and `order` directly.
                let elapsed = now_nanos().saturating_sub(last_rx);
                if Duration::from_nanos(elapsed) > idle_timeout {
                    warn!(?peer, "PVA client idle > {idle_timeout:?}; closing");
                    hb_stopped = true;
                    continue;
                }
                let h = PvaHeader::control(true, order, ControlCommand::EchoRequest.code(), 0);
                let mut buf = Vec::with_capacity(8);
                h.write_into(&mut buf);
                if tx.send(buf).await.is_err() {
                    // Writer gone. The loop-top `tx.is_closed()` check
                    // unwinds the connection on the next iteration; stop
                    // beating so this arm cannot spin in the meantime.
                    hb_stopped = true;
                }
                continue;
            }
            frame_result = read_frame(&mut reader, &mut rx_buf, op_timeout, max_msg_size) => {
                frame_result?
            }
        };
        // bytes_in counter (header + payload). Drives
        // PvaServer::report() throughput diagnostics.
        peer_entry.touch_rx(PvaHeader::SIZE + frame.payload.len());
        last_rx = now_nanos();
        if frame.header.flags.is_control() {
            // A peer may re-negotiate the connection byte order mid-stream
            // with another SET_BYTE_ORDER control frame. pvxs latches
            // `sendBE = header[2] & pva_flags::MSB` on every received
            // SetEndian (conn.cpp:169-188) and uses it for all subsequent
            // sends; old pvAccess accepts it from either peer at any time.
            // Latch it into the local owner `order` and the shared cell so
            // the next outbound frame (echo response, op replies, heartbeat)
            // adopts the new order. The flag is the control frame's own
            // header bit 7 (`frame.order()`), not the size field —
            // pvAccessCPP/Java ignore the size field and assume the
            // 0x00000000 ("use this order") behaviour.
            if frame.header.command == ControlCommand::SetByteOrder.code() {
                order = frame.order();
                out_order.store(order.is_big(), Ordering::Relaxed);
                continue;
            }
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

        // segmentation gate. Mirrors pvxs conn.cpp:228-244.
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
            seg_order = frame.order();
            seg_buf.clear();
        }
        // Cap reassembly on the *accumulated* size. read_frame enforces
        // `max_message_size` per frame; without this an adversary streams
        // SegFirst → SegMiddle … forever, every segment individually legal,
        // and seg_buf grows without bound. The server's default is a ceiling
        // (`config::DEFAULT_MAX_MESSAGE_SIZE`); `None` is the explicit opt-out
        // to pvxs's uncapped RX, and only then does this guard stand down.
        if let Some(cap) = max_msg_size {
            if seg_buf.len().saturating_add(frame.payload.len()) > cap {
                return Err(PvaError::Protocol(format!(
                    "segmented PVA message exceeds max_message_size ({} > {})",
                    seg_buf.len() + frame.payload.len(),
                    cap
                )));
            }
        }
        try_extend(&mut seg_buf, &frame.payload, "the segment-reassembly buffer")?;
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
                    flags: HeaderFlags::new(false, false, seg_order),
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
                process_connection_validation(
                    &frame,
                    &tx,
                    order,
                    x509_locked,
                    &mut cred,
                    peer,
                    &peer_entry,
                    &config,
                    &mut rx_type_cache,
                )
                .await?;
                handshake_complete = true;
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
                // spawning version. Resolver tasks run
                // has_pv() + get_introspection() in the background;
                // results arrive via cc_rx and are applied at the top
                // of the loop. `peer_entry.channel_opened()` registers the
                // channel (and its report stat) there, so we do not track
                // it here.
                handle_create_channel(
                    &source,
                    &frame,
                    &tx,
                    &channels,
                    order,
                    config.max_channels_per_connection,
                    peer,
                    &cred,
                    &cc_tx,
                    &mut pending_channel_spawns,
                )
                .await?;
            }
            Some(Command::DestroyChannel) => {
                // Teardown + report bookkeeping (`channel_closed`) is owned
                // by `finalize_channel_destroy`, reached via the validation
                // wrapper — the same owner the server-initiated invalidation
                // arm uses, so neither path can partially tear a channel down.
                let teardown = ChannelTeardownCtx {
                    tx: &tx,
                    order,
                    peer,                    peer_entry: &peer_entry,
                };
                handle_destroy_channel(&frame, &mut channels, &teardown).await?;
            }
            Some(Command::Get) => {
                peer_entry.op_init();
                handle_op(
                    &frame,
                    &tx,
                    &mut channels,
                    order,
                    &out_order,
                    OpKind::Get,
                    &config,
                    &mut encode_type_cache,
                    &mut rx_type_cache,
                    peer,
                    &cred,
                    &mon_fin_tx,
                    &exec_fin_tx,
                )
                .await?;
            }
            Some(Command::Put) => {
                peer_entry.op_init();
                handle_op(
                    &frame,
                    &tx,
                    &mut channels,
                    order,
                    &out_order,
                    OpKind::Put,
                    &config,
                    &mut encode_type_cache,
                    &mut rx_type_cache,
                    peer,
                    &cred,
                    &mon_fin_tx,
                    &exec_fin_tx,
                )
                .await?;
            }
            Some(Command::Monitor) => {
                peer_entry.op_init();
                handle_op(
                    &frame,
                    &tx,
                    &mut channels,
                    order,
                    &out_order,
                    OpKind::Monitor,
                    &config,
                    &mut encode_type_cache,
                    &mut rx_type_cache,
                    peer,
                    &cred,
                    &mon_fin_tx,
                    &exec_fin_tx,
                )
                .await?;
            }
            Some(Command::Rpc) => {
                peer_entry.op_init();
                handle_op(
                    &frame,
                    &tx,
                    &mut channels,
                    order,
                    &out_order,
                    OpKind::Rpc,
                    &config,
                    &mut encode_type_cache,
                    &mut rx_type_cache,
                    peer,
                    &cred,
                    &mon_fin_tx,
                    &exec_fin_tx,
                )
                .await?;
            }
            Some(Command::GetField) => {
                // GET_FIELD is a real IOID-keyed operation (pvxs models it as
                // `ServerIntrospect` in `opByIOID`, serverintrospect.cpp:141-178).
                // The slow path reserves its IOID in `ch.ops` before spawning
                // the introspection task and releases it through the same
                // `ExecFinished` completion owner GET/PUT/RPC use, so duplicate
                // IOIDs, DESTROY_REQUEST, and teardown are handled uniformly.
                handle_get_field(&frame, &tx, &mut channels, order, peer, &cred, &exec_fin_tx)
                    .await?;
            }
            Some(Command::Search) => {
                // TCP-circuit SEARCH (pvxs
                // `serverchan.cpp:173-255`). Required for
                // name-server-redirect deployments where pvxs
                // clients send SEARCH over the established TCP
                // connection rather than via UDP. Pre-fix Rust
                // had no arm here and the frame fell through to
                // the silent default — the redirector hung waiting
                // for SEARCH_RESPONSE.
                handle_tcp_search(&source, &frame, &tx, &config, peer).await?;
            }
            Some(Command::DestroyRequest) => {
                handle_destroy_request(&frame, &mut channels)?;
            }
            Some(Command::CancelRequest) => {
                handle_cancel_request(&frame, &mut channels)?;
            }
            Some(Command::Message) => {
                handle_message(&frame, &channels, &peer)?;
            }
            Some(Command::PutGet) => {
                // atomic put-then-get. The PVA wire spec defines
                // PUT_GET as a separate command (cmd 12). pvxs leaves
                // `handle_PUT_GET` empty, but we implement the full
                // INIT/PUT/GET/DESTROY lifecycle on the Rust side so
                // a PUT_GET-capable client gets a real round trip.
                peer_entry.op_init();
                handle_put_get(
                    &frame,
                    &tx,
                    &mut channels,
                    order,
                    &config,
                    &mut encode_type_cache,
                    &mut rx_type_cache,
                    peer,
                    &cred,
                    &exec_fin_tx,
                )
                .await?;
            }
            Some(Command::Process) => {
                // trigger record processing with no value
                // transfer (PVA cmd 16). Full INIT/PROCESS/DESTROY
                // lifecycle — routed through the source's typed
                // `process_checked` (WRITE-class ACF gate).
                peer_entry.op_init();
                handle_process(
                    &frame,
                    &tx,
                    &mut channels,
                    order,
                    &config,
                    &mut rx_type_cache,
                    peer,
                    &cred,
                    &exec_fin_tx,
                )
                .await?;
            }
            Some(Command::Array) => {
                // ChannelArray windowed-array op (PVA cmd 14). Full
                // INIT / getArray / putArray / setLength / getLength /
                // DESTROY lifecycle routed through the source's
                // `channel_array_*` methods (READ/WRITE-class ACF gates).
                // Pre-fix this command had no arm and the frame fell
                // through to the silent default — a ChannelArray client
                // hung waiting for the INIT reply. The default source
                // impl rejects with a protocol `Status` error so the
                // client always gets an answer.
                peer_entry.op_init();
                handle_channel_array(
                    &frame,
                    &tx,
                    &mut channels,
                    order,
                    &config,
                    &mut encode_type_cache,
                    &mut rx_type_cache,
                    peer,
                    &cred,
                    &exec_fin_tx,
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
            Some(Command::ConnectionValidation) => {
                // Post-handshake re-authentication. pvxs keeps
                // CONNECTION_VALIDATION in the live command switch
                // (conn.cpp:247-260) and re-runs handle_CONNECTION_VALIDATION
                // ("Client begins (restarts?) Auth handshake",
                // serverconn.cpp:196-251) on every dispatch, replacing the
                // credential and re-issuing CONNECTION_VALIDATED. Route it
                // through the same owner so the new identity takes effect for
                // subsequent ACF-gated operations (`cred` is captured by
                // reference by every later handle_* call).
                process_connection_validation(
                    &frame,
                    &tx,
                    order,
                    x509_locked,
                    &mut cred,
                    peer,
                    &peer_entry,
                    &config,
                    &mut rx_type_cache,
                )
                .await?;
            }
            _ => {
                // Unhandled — keep going.
            }
        }
        }
    }
    .await;

    // Connection teardown. Drain every still-open channel and run the
    // same op-cleanup-then-`onClose` sequence DESTROY_CHANNEL uses, so a
    // source learns about channels closed by disconnect/idle/error
    // exactly as it learns about explicit destroys (pvxs
    // `ServerChan::cleanup`, serverchan.cpp:43-60). Draining (rather than
    // letting the map's `Drop` free the ops silently) is what delivers
    // the close notification the implicit `Drop` cannot.
    for (_sid, ch) in channels.drain() {
        close_channel(ch, peer);
    }
    conn_result
}

/// Decode the VALUE body of an INIT pvRequest, after its descriptor
/// has already been read from `cur`.
///
/// pvxs `from_wire_type_value` (`dataencode.cpp:747-752`): once the
/// descriptor yields a non-null `Value`, it ALWAYS runs `from_wire_full`
/// to read the value body; a wire fault there leaves the buffer bad and
/// the caller `bev.reset()`s the connection (`serverget.cpp:371-375`,
/// `servermon.cpp:489-501`). A descriptor whose sub-structures are all
/// empty — the default `field(...)` selector — needs zero value bytes and
/// `from_wire_full` stays good, so an exhausted buffer is legal only when
/// the descriptor requires no bytes. There is no "absent value body"
/// concept: a present non-null descriptor that ends before its required
/// value bytes is the same `!M.good()` fault as a truncated one.
///
/// So decode unconditionally (no cursor short-circuit): a descriptor with
/// scalar/array leaves whose values were truncated or omitted faults here
/// instead of silently dropping the create-time `record._options`
/// (`pipeline` / `_filter` / `process` / `block` / `atomic`) and
/// registering an OK op. The previous cursor-exhausted short-circuit to
/// `Ok(None)` swallowed exactly that descriptor-only case.
///
/// The `Option` contract the create-time-option consumers rely on is
/// preserved: a content-less value body (zero bytes consumed because the
/// frame is exhausted) stays `None`; a value that actually carried bytes
/// becomes `Some`. `exhausted && Ok` implies `from_wire_full` consumed
/// nothing (only all-empty structures decode from an empty buffer), so the
/// `None` here is byte-identical to the prior contract on every path that
/// previously succeeded — only the malformed descriptor-only path flips
/// from `Ok(None)` to a connection-fatal `Err`.
fn decode_init_pv_request_value(
    cur: &mut std::io::Cursor<&[u8]>,
    req_desc: &FieldDesc,
    order: ByteOrder,
    decode_cache: &mut TypeCache,
) -> Result<Option<PvField>, String> {
    let exhausted = cur.position() as usize >= cur.get_ref().len();
    let value = decode_pv_field_cached(req_desc, cur, order, decode_cache)
        .map_err(|e| format!("invalid pvRequest value: {e}"))?;
    Ok(if exhausted { None } else { Some(value) })
}

/// Decode an INIT pvRequest (`type + full value`) — the single owner of the
/// INIT pvRequest wire shape for every op kind (GET / PUT / MONITOR / RPC /
/// PUT_GET / PROCESS / ARRAY).
///
/// pvxs `from_wire_type_value` (`dataencode.cpp:747-753`):
///
/// ```c
/// from_wire_type(buf, ctxt, val);
/// if(buf.good() && val)
///     from_wire_full(buf, ctxt, val);
/// ```
///
/// A NULL (`0xFF`) type code is legal here and is NOT a wire fault: it leaves
/// the descriptor list empty with the buffer still good (`dataencode.cpp:
/// 79-80`), `from_wire_type` yields an invalid `Value` (`:737-744`), and the
/// `if(... && val)` guard skips the value body. The INIT then passes
/// `serverget.cpp:366-376` / `servermon.cpp:491-503`, which check only
/// `!M.good()`, and the invalid pvRequest becomes the all-fields wildcard in
/// `request2mask` (`pvrequest.cpp:53-55`). It is the exact byte pvxs's own
/// `to_wire(Buf&, const FieldDesc*)` writes for a null descriptor
/// (`dataencode.cpp:29-33`).
///
/// Rejecting it (as `decode_type_desc_cached` does, by design) tore down the
/// whole TCP circuit — killing every other channel and operation multiplexed
/// on it — where pvxs replies with a normal INIT success.
///
/// A present-but-malformed descriptor, or a non-null descriptor whose value
/// body is truncated, is still the `!M.good()` fault the callers turn into a
/// connection reset.
fn decode_init_pv_request(
    cur: &mut std::io::Cursor<&[u8]>,
    order: ByteOrder,
    decode_cache: &mut TypeCache,
) -> Result<(Option<FieldDesc>, Option<PvField>), String> {
    let Some(req_desc) =
        crate::pvdata::encode::decode_type_desc_cached_opt(cur, order, decode_cache)
            .map_err(|e| format!("invalid pvRequest descriptor: {e}"))?
    else {
        return Ok((None, None));
    };
    let req_value = decode_init_pv_request_value(cur, &req_desc, order, decode_cache)?;
    Ok((Some(req_desc), req_value))
}

/// Decode an RPC EXEC argument body (`type + full value`), keeping the
/// "parameterless" case structurally distinct from the "malformed"
/// case.
///
/// pvxs `serverget.cpp:443-447` decodes RPC EXEC with
/// `from_wire_type_value`, then `serverget.cpp:454-458` resets the
/// connection when that decode leaves the message bad. The underlying
/// `from_wire_type` (`dataencode.cpp:729-745`) reads a single type-code
/// byte: a NULL (`0xFF`) type code yields a null `Value` — a
/// parameterless RPC — with the buffer still good, while a present
/// non-null descriptor is decoded in full and any underflow faults the
/// buffer.
///
/// The previous inline `match decode_type_desc { Err(_) => Null }` gave
/// `decode_type_desc`'s error a dual meaning: it stood for both "absent
/// body / NULL type code (parameterless)" and "present but undecodable
/// descriptor (must be fatal)". That conflation let a truncated or
/// corrupt RPC EXEC frame reach the application handler as a fabricated
/// `Null` argument. This helper gives each path one meaning:
///
/// - `Ok((FieldDesc::Variant, PvField::Null))` for an absent body or a
///   NULL (`0xFF`) type code — a parameterless RPC. (pvxs encodes a
///   parameterless RPC as the single `0xFF` byte written by
///   `clientget.cpp:308` `to_wire(R, desc(arg))` when `arg` is null;
///   `decode_type_desc` rejects `0xFF` as caller-context dependent, so
///   it is peek-handled here. An empty body is additionally tolerated
///   for Rust↔Rust interop, where the client may send no payload after
///   subcmd.)
/// - `Ok((desc, value))` for a present, fully decoded descriptor +
///   value.
/// - `Err(message)` for a present-but-malformed descriptor or value;
///   the caller turns that into a connection-fatal decode error,
///   matching pvxs `bev.reset()`.
fn decode_rpc_exec_arg(
    cur: &mut std::io::Cursor<&[u8]>,
    order: ByteOrder,
    decode_cache: &mut TypeCache,
) -> Result<(FieldDesc, PvField), String> {
    // Absent body (no payload after subcmd): parameterless RPC.
    if cur.position() as usize >= cur.get_ref().len() {
        return Ok((FieldDesc::Variant, PvField::Null));
    }
    // NULL (0xFF) type code: parameterless RPC (pvxs interop). Routed through
    // `decode_type_desc_cached_opt`, the single owner of the NULL-type rule,
    // rather than a local peek.
    let Some(desc) = crate::pvdata::encode::decode_type_desc_cached_opt(cur, order, decode_cache)
        .map_err(|e| format!("invalid RPC argument descriptor: {e}"))?
    else {
        return Ok((FieldDesc::Variant, PvField::Null));
    };
    // Present, non-null descriptor: decode the full value or fail fatally —
    // a present-but-undecodable body is a protocol error, not a
    // parameterless call.
    let value = decode_pv_field_cached(&desc, cur, order, decode_cache)
        .map_err(|e| format!("invalid RPC argument value: {e}"))?;
    Ok((desc, value))
}

/// Build a minimal [`OpState`] for non-MONITOR ops (GET / PUT /
/// PUT_GET / PROCESS). The monitor-specific fields are all defaulted
/// to inert values — these ops never spawn a subscriber task.
fn non_monitor_op_state(intro: Arc<FieldDesc>, kind: OpKind, mask: BitSet) -> OpState {
    OpState {
        intro,
        kind,
        monitor_started: false,
        monitor_abort: None,
        mask,
        put_mask: None,
        monitor_window: None,
        monitor_window_notify: None,
        monitor_paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        monitor_resume: Arc::new(tokio::sync::Notify::new()),
        monitor_wm: None,
        monitor_wm_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        monitor_op_id: next_op_id(),
        monitor_filters: Arc::new(epics_base_rs::server::database::filters::FilterChain::new()),
        pv_request: None,
        monitor_options: crate::server_native::source::MonitorOptions::default(),
        data_task_abort: None,
        monitor_start_ctl: None,
        exec_state: ExecState::Idle,
        last_request: false,
    }
}

/// PVA `PUT_GET` (cmd 12) handler — atomic put-then-get.
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
///   The command-local last-request bit (`subcmd & 0x10`, `QOS_DESTROY`)
///   is the EPICS `lastRequest()` rider — the ChannelPutGet client sends
///   `QOS_DESTROY` to mean "run this, then destroy the op"
///   (`clientContextImpl.cpp:1262-1288`), NOT a standalone destroy. It must
///   still execute and reply; the op is freed only after the reply, via the
///   same `finish_exec_data_task` path GET/PUT/RPC use. Standalone op
///   destruction is `CMD_DESTROY_REQUEST`, handled elsewhere.
///
/// pvxs leaves `handle_PUT_GET` empty; this implements the operation
/// properly per the wire spec so a PUT_GET-capable client works.
#[allow(clippy::too_many_arguments)]
async fn handle_put_get(
    frame: &Frame,
    tx: &SrvTx,
    channels: &mut HashMap<u32, ChannelState>,
    order: ByteOrder,
    config: &PvaServerConfig,
    encode_cache: &mut EncodeTypeCache,
    // Connection-scope inbound decode cache (pvxs `rxRegistry`, conn.h:23).
    decode_cache: &mut TypeCache,
    peer: std::net::SocketAddr,
    cred: &ClientCredentials,
    // data-phase-completion sender (see [`handle_op`]). The spawned
    // PUT_GET exec task installs an `ExecFinishGuard` so the owner returns
    // the op to `Idle` when its readback reply is sent.
    exec_fin_tx: &mpsc::UnboundedSender<ExecFinished>,
) -> PvaResult<()> {
    // Inbound payload decodes with the frame's own header order (pvxs
    // latches `peerBE` per received message, conn.cpp:195-198); `order`
    // (config) is used only for outbound reply frames.
    let inbound_order = frame.order();
    let mut cur = frame.cursor();
    let sid = cur
        .get_u32(inbound_order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let ioid = cur
        .get_u32(inbound_order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let subcmd = cur.get_u8().map_err(|e| PvaError::Decode(e.to_string()))?;

    // PUT_GET (cmd 12) is a Rust extension — pvxs leaves `handle_PUT_GET`
    // an empty stub (serverconn.cpp:259-260) and its client never sends
    // cmd 12 (clientimpl.h:143). `serve_put_get` (default true) gates
    // whether this server serves the operation; a deployment that wants
    // strict pvxs-compatible behavior sets it false and every cmd-12
    // frame — putGet, getGet, getPut — is answered with a deterministic
    // error `Status` rather than the Rust round trip. We reply with an
    // explicit error (not pvxs's silent drop) so the policy is visible at
    // the wire level and a client fails fast instead of waiting out its
    // `op_timeout`. Gated before the channel borrow, like the unknown-sid
    // path below, so it needs no resolved channel.
    if !config.serve_put_get {
        send_op_error(
            tx,
            OpKind::PutGet,
            ioid,
            subcmd,
            Status::error("PUT_GET not served (serve_put_get disabled for pvxs compatibility)"),
            order,
        )
        .await?;
        return Ok(());
    }

    // Connection-wide IOID uniqueness (pvxs `ServerConn::opByIOID`),
    // evaluated before the per-channel borrow below.
    let dup_ioid = subcmd & QosFlags::INIT != 0 && ioid_live_on_conn(channels, ioid);

    // Data-phase frames resolve their channel via the connection-wide op
    // owner, not the frame SID (see `data_phase_owner_sid` / `handle_op`).
    // PUT_GET is a single request-response op (GPR-like), so the frame SID
    // is not re-validated; INIT keeps the frame's channel.
    let sid = if subcmd & QosFlags::INIT != 0 {
        sid
    } else {
        data_phase_owner_sid(channels, ioid, sid, false)?.unwrap_or(sid)
    };

    let ch = match channels.get_mut(&sid) {
        Some(c) => c,
        None => {
            send_op_error(
                tx,
                OpKind::PutGet,
                ioid,
                subcmd,
                Status::error("unknown channel sid"),
                order,
            )
            .await?;
            return Ok(());
        }
    };

    // Attribute the inbound PUT_GET frame to the channel and route every
    // reply through `chan_tx` (pvxs `chan->statRx`/`statTx`; see
    // `handle_op`).
    ch.stat.add_op_rx(frame);
    let chan_tx = ChannelTx::new(tx.clone(), ch.stat.clone());

    // Dispatch to the CREATE_CHANNEL-bound owner, not the registry
    // (pvxs serverchan.cpp:70-112 / server.cpp:100-112; see `handle_op`).
    let source = ch.source.clone();
    let source = &source;

    if subcmd & QosFlags::INIT != 0 {
        // duplicate INIT on a live IOID is connection-fatal
        // (mirror of `handle_op`; connection-wide scope per pvxs `opByIOID`).
        if dup_ioid {
            return Err(PvaError::Decode(format!(
                "duplicate PUT_GET INIT on live IOID {ioid}"
            )));
        }
        if ch.ops.len() >= config.max_ops_per_channel {
            send_chan_op_error(
                &chan_tx,
                OpKind::PutGet,
                ioid,
                subcmd,
                Status::error("max ops per channel exceeded"),
                order,
            )
            .await?;
            return Ok(());
        }
        // PUT_GET also requires a descriptor.
        let intro = match ch.introspection.clone() {
            Some(d) => d,
            None => {
                send_chan_op_error(
                    &chan_tx,
                    OpKind::PutGet,
                    ioid,
                    subcmd,
                    Status::error("must provide prototype"),
                    order,
                )
                .await?;
                return Ok(());
            }
        };
        // pvRequest: `type + value` (pvxs clientget.cpp). Translate to
        // a field mask the GET leg consults.
        // A peer wire-decode fault of the INIT pvRequest type/value is
        // connection-fatal (pvxs `serverget.cpp:371-375` bev.reset()), not
        // a per-op Status — the empty-mask case below stays op-level.
        let (req_desc, req_value) =
            match decode_init_pv_request(&mut cur, inbound_order, decode_cache) {
                Ok(v) => v,
                Err(e) => {
                    return Err(PvaError::Decode(format!("PUT_GET INIT pvRequest: {e}")));
                }
            };
        // ChannelPutGet negotiates TWO field selections at INIT: the
        // put-leg (`putField`) and the get-leg (`getField`). pvDatabaseCPP
        // `ChannelPutGetLocal::create` builds a separate PVCopy for each
        // (modules/pvDatabase/src/pvAccess/channelLocal.cpp), so `getPut`
        // reads back the put-leg structure and `putGet`/`getGet` the
        // get-leg structure. We derive both masks here. When the pvRequest
        // carries no putField/getField the pvAccess `getRequestedStructure`
        // fallback collapses both to the common `field` selection
        // (modules/pvAccess/testApp/remote/testServer.cpp), so the common NT
        // round trip is unchanged. An empty selection is an INIT error.
        let (put_mask, get_mask) = match crate::pv_request::put_get_masks(&intro, req_desc.as_ref())
        {
            Ok(masks) => masks,
            Err(e) => {
                send_chan_op_error(
                    &chan_tx,
                    OpKind::PutGet,
                    ioid,
                    subcmd,
                    Status::error(format!("invalid pvRequest mask: {e}")),
                    order,
                )
                .await?;
                return Ok(());
            }
        };

        // stash the INIT pvRequest so the data phase can
        // forward `record._options` (process/block, group `atomic`)
        // through `ChannelContext.pv_request` to the source. The
        // dedicated PUT_GET path otherwise dropped it, so QSRV group
        // PUT_GET could not honor INIT options on the native wire.
        //
        // The get-leg mask is the op's primary selection mask (`OpState.mask`,
        // drives the putGet/getGet readback); the put-leg mask rides in
        // `OpState.put_mask` for getPut.
        let mut put_get_op = non_monitor_op_state(intro.clone(), OpKind::PutGet, get_mask);
        put_get_op.put_mask = Some(put_mask);
        put_get_op.pv_request = req_value;
        ch.ops.insert(ioid, put_get_op);

        // INIT response: ioid + subcmd + status + putIF + getIF.
        // pvAccessJava protocol defines PUT_GET INIT with two type
        // descriptors (put-request structure, then get-response structure).
        // pvxs never implements PUT_GET (`handle_PUT_GET` is an empty stub
        // in serverconn.cpp). We serve the same channel introspection for
        // both legs.
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
        let _ = chan_tx.send(buf).await;
        return Ok(());
    }

    // PUT-GET data phase.
    let op = ch.ops.get(&ioid).cloned();
    let (intro, mask, put_mask, init_pv_request) = match op {
        Some(o) => {
            // the data-phase command must match the operation
            // kind bound at INIT. pvxs `serverget.cpp:421-436` resets
            // the connection when an IOID is driven by the wrong
            // operation class. Without this check a client could INIT
            // an IOID as a GET/PUT/MONITOR and then drive a dedicated
            // PUT_GET data frame through it, performing a
            // write/readback with a descriptor + mask the operation
            // never negotiated as a PUT_GET. Mirror the generic
            // handler's protocol-error policy (tcp.rs data-phase
            // kind guard).
            if o.kind != OpKind::PutGet {
                return Err(PvaError::Decode(format!(
                    "PUT_GET data-phase frame for IOID {ioid} initialised as {:?} \
                     (pvxs serverget.cpp:421-436 protocol error)",
                    o.kind
                )));
            }
            (o.intro, o.mask, o.put_mask, o.pv_request)
        }
        None => {
            send_chan_op_error(
                &chan_tx,
                OpKind::PutGet,
                ioid,
                subcmd,
                Status::error("operation not initialised"),
                order,
            )
            .await?;
            return Ok(());
        }
    };
    let pv_name = ch.name.clone();

    // EPICS ChannelPutGet has three data-phase subcommands sharing the
    // PUT_GET command byte (pvAccess remote.h:78-82): the default putGet
    // (0x00) writes then reads back, getGet (`QOS_GET`, 0x40) reads the
    // current get-side data, and getPut (`QOS_GET_PUT`, 0x80) reads the
    // current put-side data. Only putGet carries a put payload —
    // clientContextImpl.cpp:1100-1112 serializes the put BitSet + value
    // solely for the default branch and sends nothing for getGet/getPut.
    // Decoding a BitSet/value for those payload-less frames is what made
    // the operation fail, so gate both the decode and the write leg on the
    // subcommand.
    let read_only = subcmd & (QosFlags::GET | QosFlags::GET_PUT) != 0;

    // The two read-back legs project distinct structures: getPut returns the
    // put-leg (`putField`) structure's current value, putGet/getGet the
    // get-leg (`getField`) structure's value (pvDatabaseCPP
    // `ChannelPutGetLocal::getPut`/`getGet`,
    // modules/pvDatabase/src/pvAccess/channelLocal.cpp). Both read the same
    // backing value but mask it by their own selection — the INIT serves the
    // full introspection for both descriptors, so the leg is carried by the
    // changed-bitset rather than a projected type (uniform with GET/PUT/
    // MONITOR). `mask` is the get-leg mask; `put_mask` the put-leg mask (set
    // for every PUT_GET op at INIT, falling back to `mask` defensively).
    let readback_mask = if subcmd & QosFlags::GET_PUT != 0 {
        put_mask.unwrap_or_else(|| mask.clone())
    } else {
        mask.clone()
    };

    // Decode the put payload inline (cursor is borrowed from the stack
    // frame) only for the write/readback putGet path.
    let put_payload = if read_only {
        None
    } else {
        let changed =
            BitSet::decode(&mut cur, inbound_order).map_err(|e| PvaError::Decode(e.to_string()))?;
        let put_delta = decode_pv_field_with_bitset_cached(
            &intro,
            &changed,
            0,
            &mut cur,
            inbound_order,
            decode_cache,
        )
        .map_err(|e| PvaError::Decode(format!("PUT_GET requires a value payload: {e}")))?;
        Some((changed, put_delta))
    };

    let ctx = crate::server_native::source::ChannelContext {
        peer,
        account: cred.account.clone(),
        method: cred.method.clone(),
        host: cred.host.clone(),
        authority: cred.authority.clone(),
        roles: cred.roles.clone(),
        pv_request: init_pv_request,
        log: Default::default(),
    };

    let src = source.clone();
    let tx_clone = chan_tx.clone();
    // run this PUT_GET exec only when the op is `Idle`, and ignore a
    // second EXEC while the first is in flight rather than aborting it (pvxs
    // `serverget.cpp:467-476`/`:511-514`).
    let op_id = match begin_exec(ch, ioid) {
        Some(id) => id,
        None => {
            debug!(ioid, "PUT_GET EXEC ignored: op already executing");
            return Ok(());
        }
    };
    let exec_fin = ExecFinished {
        sid,
        ioid,
        op_id,
        success: false,
    };
    let exec_fin_tx_task = exec_fin_tx.clone();
    let abort = poll_inline_or_spawn(async move {
        // return this op to `Idle` (via the read-loop owner) when the
        // task ends so a later explicit re-EXEC is accepted.
        let _exec_fin_guard = ExecFinishGuard {
            tx: exec_fin_tx_task,
            fin: exec_fin,
        };
        let mut payload = Vec::new();
        payload.put_u32(ioid, order);
        payload.put_u8(subcmd);
        // The source's `RemoteLogger` sink for this op, kept alive past the
        // moves below so its diagnostics can be flushed before the reply.
        let op_log = ctx.log.clone();

        // putGet (0x00) is the atomic WRITE+READ round trip; getGet/getPut
        // (read-only) carry no put payload and only read back. The default
        // `put_get_checked` composes put_delta_checked + get_value_checked
        // over the same backing store, but a remote-fronting source (the
        // pva-gateway) overrides it to issue ONE upstream PUT_GET so the
        // put-then-get stays atomic upstream (pva2pva
        // `p2pApp/channel.cpp:129-137`) instead of a local put plus a
        // separately-read cached get. The read-only legs stay a plain
        // READ-gated `get_value_checked`. A panic in either user handler
        // becomes an error reply instead of skipping the reply below.
        // The error slot holds the `Status` this reply will carry, not text:
        // a source fronting a remote (the gateway) reports its upstream's own
        // Status, and rendering it to a string here is what put a Rust `{:?}`
        // dump on the wire (R18-27).
        let read_value: Result<Option<crate::server_native::source::SourceRead>, Status> =
            if let Some((changed, put_delta)) = put_payload {
                let checked = src
                    .access_gate()
                    .check_with_roles(
                        &pv_name,
                        &ctx.host,
                        &ctx.account,
                        &ctx.roles,
                        &ctx.method,
                        &ctx.authority,
                    )
                    .await;
                match catch_handler_panic(src.put_get_checked(
                    checked,
                    intro.clone(),
                    changed,
                    put_delta,
                    ctx.clone(),
                ))
                .await
                {
                    Ok(Ok(v)) => Ok(v),
                    Ok(Err(e)) => Err(e.wire_status()),
                    Err(panic) => Err(Status::error(panic)),
                }
            } else {
                let read_checked = src
                    .access_gate()
                    .check_with_roles(
                        &pv_name,
                        &ctx.host,
                        &ctx.account,
                        &ctx.roles,
                        &ctx.method,
                        &ctx.authority,
                    )
                    .await;
                catch_handler_panic(src.read_checked(read_checked, ctx))
                    .await
                    .map_err(Status::error)
            };

        flush_remote_log(&op_log, ioid, order, &tx_clone).await;

        match read_value {
            Ok(Some(read)) => {
                Status::ok().write_into(order, &mut payload);
                // Project the read-back value by this leg's selection mask
                // (getPut → put-leg, putGet/getGet → get-leg). See the
                // `readback_mask` derivation above), narrowed to the leaves
                // the source actually assigned — pvxs `to_wire_valid(R, value,
                // &pvMask)` frames the readback exactly like a GET reply.
                let wire_changed =
                    read_changed_bitset(&intro, &readback_mask, read.marked.as_deref());
                wire_changed.write_into(order, &mut payload);
                crate::pvdata::encode::encode_pv_field_with_bitset(
                    &read.value,
                    &intro,
                    &wire_changed,
                    0,
                    order,
                    &mut payload,
                );
            }
            Ok(None) => {
                Status::ok().write_into(order, &mut payload);
                let empty = BitSet::with_capacity(intro.total_bits());
                empty.write_into(order, &mut payload);
            }
            Err(status) => status.write_into(order, &mut payload),
        }
        let h = PvaHeader::application(true, order, Command::PutGet.code(), payload.len() as u32);
        let mut buf = Vec::new();
        h.write_into(&mut buf);
        buf.extend_from_slice(&payload);
        let _ = tx_clone.send(buf).await;
    });
    // Store the abort guard and, when this PUT_GET frame carried the
    // last-request bit (`subcmd & 0x10`), defer the op's removal until its
    // reply has been sent — the same completion-owned cleanup GET/PUT/RPC use
    // (see [`finish_exec_data_task`]).
    finish_exec_data_task(ch, ioid, subcmd, abort);
    Ok(())
}

/// PVA `PROCESS` (cmd 16) handler — trigger record processing
/// with no value transfer.
///
/// Sub-command lifecycle:
/// - INIT  (`subcmd & 0x08`): decode + discard the pvRequest, register
///   the op, reply `ioid + subcmd + status` (no introspection — there
///   is no value type to negotiate).
/// - PROCESS (`subcmd & 0x08 == 0`): run the WRITE-gated
///   `process_checked` on the source, reply `ioid + subcmd + status`.
///   The command-local last-request bit (`subcmd & 0x10`, `QOS_DESTROY`)
///   is the EPICS `lastRequest()` rider — the ChannelProcess client sends
///   `QOS_DESTROY` to mean "process this, then destroy the op"
///   (`clientContextImpl.cpp:548-570`), NOT a standalone destroy. It must
///   still execute and reply; the op is freed only after the reply, via
///   the same `finish_exec_data_task` path GET/PUT/RPC use. Standalone op
///   destruction is `CMD_DESTROY_REQUEST`, handled elsewhere.
#[allow(clippy::too_many_arguments)]
async fn handle_process(
    frame: &Frame,
    tx: &SrvTx,
    channels: &mut HashMap<u32, ChannelState>,
    order: ByteOrder,
    config: &PvaServerConfig,
    // Connection-scope inbound decode cache (pvxs `rxRegistry`, conn.h:23).
    // PROCESS transfers no value but its INIT pvRequest descriptor is still
    // decoded, so it shares the same cache as every other inbound decode.
    decode_cache: &mut TypeCache,
    peer: std::net::SocketAddr,
    cred: &ClientCredentials,
    // data-phase-completion sender (see [`handle_op`]). The spawned
    // PROCESS exec task installs an `ExecFinishGuard` so the owner returns
    // the op to `Idle` when its reply is sent.
    exec_fin_tx: &mpsc::UnboundedSender<ExecFinished>,
) -> PvaResult<()> {
    // Inbound payload decodes with the frame's own header order (pvxs
    // latches `peerBE` per received message, conn.cpp:195-198); `order`
    // (config) is used only for outbound reply frames.
    let inbound_order = frame.order();
    let mut cur = frame.cursor();
    let sid = cur
        .get_u32(inbound_order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let ioid = cur
        .get_u32(inbound_order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let subcmd = cur.get_u8().map_err(|e| PvaError::Decode(e.to_string()))?;

    // Connection-wide IOID uniqueness (pvxs `ServerConn::opByIOID`),
    // evaluated before the per-channel borrow below.
    let dup_ioid = subcmd & QosFlags::INIT != 0 && ioid_live_on_conn(channels, ioid);

    // Data-phase frames resolve their channel via the connection-wide op
    // owner, not the frame SID (see `data_phase_owner_sid` / `handle_op`).
    // PROCESS is a single request-response op (GPR-like), so the frame SID
    // is not re-validated; INIT keeps the frame's channel.
    let sid = if subcmd & QosFlags::INIT != 0 {
        sid
    } else {
        data_phase_owner_sid(channels, ioid, sid, false)?.unwrap_or(sid)
    };

    let ch = match channels.get_mut(&sid) {
        Some(c) => c,
        None => {
            send_op_error(
                tx,
                OpKind::Process,
                ioid,
                subcmd,
                Status::error("unknown channel sid"),
                order,
            )
            .await?;
            return Ok(());
        }
    };

    // Attribute the inbound PROCESS frame to the channel and route every
    // reply through `chan_tx` (pvxs `chan->statRx`/`statTx`; see
    // `handle_op`).
    ch.stat.add_op_rx(frame);
    let chan_tx = ChannelTx::new(tx.clone(), ch.stat.clone());

    // Dispatch to the CREATE_CHANNEL-bound owner, not the registry
    // (pvxs serverchan.cpp:70-112 / server.cpp:100-112; see `handle_op`).
    let source = ch.source.clone();
    let source = &source;

    if subcmd & QosFlags::INIT != 0 {
        // duplicate INIT on a live IOID is connection-fatal
        // (connection-wide scope per pvxs `opByIOID`).
        if dup_ioid {
            return Err(PvaError::Decode(format!(
                "duplicate PROCESS INIT on live IOID {ioid}"
            )));
        }
        if ch.ops.len() >= config.max_ops_per_channel {
            send_chan_op_error(
                &chan_tx,
                OpKind::Process,
                ioid,
                subcmd,
                Status::error("max ops per channel exceeded"),
                order,
            )
            .await?;
            return Ok(());
        }
        // PROCESS still requires a descriptor — even though
        // PROCESS has no value payload, the source must commit to
        // *some* introspection at channel creation. A missing
        // descriptor means the source can't describe what PROCESS
        // would act on.
        let intro = match ch.introspection.clone() {
            Some(d) => d,
            None => {
                send_chan_op_error(
                    &chan_tx,
                    OpKind::Process,
                    ioid,
                    subcmd,
                    Status::error("must provide prototype"),
                    order,
                )
                .await?;
                return Ok(());
            }
        };
        // route the PROCESS INIT pvRequest through the SAME
        // structured boundary as the generic GET/PUT/MONITOR INIT path
        // (`decode_init_pv_request`). PROCESS transfers no value,
        // but the create-time pvRequest carries `record._options` a
        // provider can interpret (and a gateway must forward through
        // `createChannelProcess(..., pvRequest)`, pva2pva
        // channel.cpp:98-106), so it is preserved into the op state and
        // surfaced as `ChannelContext.pv_request` at EXEC. A present-but-
        // malformed pvRequest is a peer wire-decode fault. The previous
        // `decode_type_desc(..).ok().and_then(|d| decode_pv_field(..)
        // .ok())` collapsed "absent body", "malformed descriptor", and
        // "malformed value" all into a silent no-op, so a truncated/
        // corrupt PROCESS INIT was acknowledged with `Status::ok()` and
        // the op registered. Mirror pvxs `from_wire_type_value` +
        // `if(!M.good()) bev.reset()` (serverget.cpp:371-375): a malformed
        // descriptor or value is connection-fatal (the read loop closes
        // the circuit, no op reply), uniform with the generic INIT path.
        // A non-null descriptor that needs value bytes but ends before them
        // is the same fault; only the all-empty-structs default selector
        // (and the NULL `0xFF` descriptor) legitimately has a 0-byte body.
        let (_req_desc, req_value) =
            match decode_init_pv_request(&mut cur, inbound_order, decode_cache) {
                Ok(v) => v,
                Err(e) => {
                    return Err(PvaError::Decode(format!("PROCESS INIT pvRequest: {e}")));
                }
            };
        let mask = BitSet::all_set(intro.total_bits());
        let mut process_op = non_monitor_op_state(intro, OpKind::Process, mask);
        process_op.pv_request = req_value;
        ch.ops.insert(ioid, process_op);

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
        let _ = chan_tx.send(buf).await;
        return Ok(());
    }

    // PROCESS data phase — no payload to decode.
    let init_pv_request = match ch.ops.get(&ioid) {
        None => {
            // silently drop — pvxs serverget.cpp:423-428 and servermon.cpp:611-619
            // return without reply here to handle the DESTROY_REQUEST race.
            return Ok(());
        }
        Some(o) => {
            // the data-phase command must match the operation
            // kind bound at INIT. pvxs `serverget.cpp:421-436` resets
            // the connection when an IOID is driven by the wrong
            // operation class. Without this check any live IOID on
            // the channel could be driven into record processing via
            // a dedicated PROCESS data frame. Mirror the generic
            // handler's protocol-error policy.
            if o.kind != OpKind::Process {
                return Err(PvaError::Decode(format!(
                    "PROCESS data-phase frame for IOID {ioid} initialised as {:?} \
                     (pvxs serverget.cpp:421-436 protocol error)",
                    o.kind
                )));
            }
            o.pv_request.clone()
        }
    };
    let pv_name = ch.name.clone();
    let ctx = crate::server_native::source::ChannelContext {
        peer,
        account: cred.account.clone(),
        method: cred.method.clone(),
        host: cred.host.clone(),
        authority: cred.authority.clone(),
        roles: cred.roles.clone(),
        // PROCESS INIT pvRequest, preserved from the op state so a source
        // — and a gateway forwarding createChannelProcess(..., pvRequest)
        // — can inspect `record._options`.
        pv_request: init_pv_request,
        log: Default::default(),
    };
    let src = source.clone();
    let tx_clone = chan_tx.clone();
    // run this PROCESS exec only when the op is `Idle`, and ignore a
    // second EXEC while the first is in flight rather than aborting it (pvxs
    // `serverget.cpp:467-476`/`:511-514`).
    let op_id = match begin_exec(ch, ioid) {
        Some(id) => id,
        None => {
            debug!(ioid, "PROCESS EXEC ignored: op already executing");
            return Ok(());
        }
    };
    let exec_fin = ExecFinished {
        sid,
        ioid,
        op_id,
        success: false,
    };
    let exec_fin_tx_task = exec_fin_tx.clone();
    let abort = poll_inline_or_spawn(async move {
        // return this op to `Idle` (via the read-loop owner) when the
        // task ends so a later explicit re-EXEC is accepted.
        let _exec_fin_guard = ExecFinishGuard {
            tx: exec_fin_tx_task,
            fin: exec_fin,
        };
        let checked = src
            .access_gate()
            .check_with_roles(
                &pv_name,
                &ctx.host,
                &ctx.account,
                &ctx.roles,
                &ctx.method,
                &ctx.authority,
            )
            .await;
        // a panic in the user PROCESS handler becomes an error
        // reply instead of skipping the reply below.
        let op_log = ctx.log.clone();
        let result = catch_handler_panic(src.process_checked(checked, ctx))
            .await
            .map_err(|e| OpError::failed(e))
            .and_then(|r| r);
        flush_remote_log(&op_log, ioid, order, &tx_clone).await;

        let mut payload = Vec::new();
        payload.put_u32(ioid, order);
        payload.put_u8(subcmd);
        match result {
            Ok(()) => Status::ok().write_into(order, &mut payload),
            Err(e) => e.wire_status().write_into(order, &mut payload),
        }
        let h = PvaHeader::application(true, order, Command::Process.code(), payload.len() as u32);
        let mut buf = Vec::new();
        h.write_into(&mut buf);
        buf.extend_from_slice(&payload);
        let _ = tx_clone.send(buf).await;
    });
    // Store the abort guard and, when this PROCESS frame carried the
    // last-request bit (`subcmd & 0x10`), defer the op's removal until its
    // reply has been sent — the same completion-owned cleanup GET/PUT/RPC use
    // (see [`finish_exec_data_task`]).
    finish_exec_data_task(ch, ioid, subcmd, abort);
    Ok(())
}

/// One decoded ChannelArray data-phase sub-operation, selected by the
/// `subcmd` QOS bits (pvAccessCPP `responseHandlers.cpp:2148-2207`). The
/// windowing parameters and put value are decoded from the frame *before*
/// the exec task is spawned, so the task moves owned values rather than
/// borrowing the frame cursor.
enum ChannelArraySubOp {
    /// `getArray` (`QOS_GET`): read the `[offset, count, stride]` slice.
    Get {
        offset: u32,
        count: u32,
        stride: u32,
    },
    /// `putArray` (no get/length bits): splice `value` at `offset`/`stride`.
    Put {
        offset: u32,
        stride: u32,
        value: PvField,
    },
    /// `setLength` (`QOS_GET_PUT`): resize to `length`.
    SetLength { length: u32 },
    /// `getLength` (`QOS_PROCESS`): query the current element count.
    GetLength,
}

/// The success body a ChannelArray sub-op contributes to its reply after
/// the leading `ioid + subcmd + Status` (pvAccessCPP
/// `responseHandlers.cpp:2368-2385`).
enum ChannelArrayReply {
    /// `putArray` / `setLength`: status only, no trailing body.
    Empty,
    /// `getArray`: the sliced array value, serialised full (no BitSet).
    Value(PvField),
    /// `getLength`: the element count as a PVA `Size`.
    Length(u32),
}

/// PVA `ARRAY` (cmd 14) handler — ChannelArray windowed-array operation.
///
/// Sub-command lifecycle (QOS bits, pvAccessCPP
/// `responseHandlers.cpp:2115-2208` request decode /
/// `:2347-2393` reply encode; client `clientContextImpl.cpp:1567-1666`):
/// - INIT (`QOS_INIT 0x08`): decode the pvRequest selecting the array
///   field, call [`ChannelSource::channel_array_init`](crate::server_native::ChannelSource::channel_array_init), and reply
///   `ioid + subcmd + status [+ array introspection]`. The default source
///   refuses with a protocol `Status` error — the client always gets an
///   answer, never the pre-fix silent drop.
/// - getArray (`QOS_GET 0x40`): `offset + count + stride` → READ-gated
///   [`ChannelSource::channel_array_get`](crate::server_native::ChannelSource::channel_array_get) → `status + array value`.
/// - setLength (`QOS_GET_PUT 0x80`): `length` → WRITE-gated
///   [`ChannelSource::channel_array_set_length`](crate::server_native::ChannelSource::channel_array_set_length) → `status`.
/// - getLength (`QOS_PROCESS 0x04`): no payload → READ-gated
///   [`ChannelSource::channel_array_get_length`](crate::server_native::ChannelSource::channel_array_get_length) → `status + length`.
/// - putArray (no get/length bits): `offset + stride + array value` →
///   WRITE-gated [`ChannelSource::channel_array_put`](crate::server_native::ChannelSource::channel_array_put) → `status`.
///
/// The op stays alive across sub-ops (pvAccessCPP keeps one
/// `ChannelArray`), reusing the same `begin_exec` / `ExecFinishGuard` /
/// `finish_exec_data_task` completion owner GET/PUT/PROCESS use; a
/// `QOS_DESTROY 0x10` rider on any sub-op frees the op after its reply.
/// The INIT pvRequest is stashed in the op state and re-supplied on every
/// sub-op through `ChannelContext.pv_request`, so the source knows which
/// array field to act on without the trait holding per-op state.
#[allow(clippy::too_many_arguments)]
async fn handle_channel_array(
    frame: &Frame,
    tx: &SrvTx,
    channels: &mut HashMap<u32, ChannelState>,
    order: ByteOrder,
    config: &PvaServerConfig,
    encode_cache: &mut EncodeTypeCache,
    decode_cache: &mut TypeCache,
    peer: std::net::SocketAddr,
    cred: &ClientCredentials,
    exec_fin_tx: &mpsc::UnboundedSender<ExecFinished>,
) -> PvaResult<()> {
    // Inbound payload decodes with the frame's own header order; `order`
    // (config) drives only outbound reply frames (see [`handle_process`]).
    let inbound_order = frame.order();
    let mut cur = frame.cursor();
    let sid = cur
        .get_u32(inbound_order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let ioid = cur
        .get_u32(inbound_order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let subcmd = cur.get_u8().map_err(|e| PvaError::Decode(e.to_string()))?;

    let dup_ioid = subcmd & QosFlags::INIT != 0 && ioid_live_on_conn(channels, ioid);

    // Data-phase frames resolve their channel via the connection-wide op
    // owner, not the frame SID (see `data_phase_owner_sid` / `handle_op`).
    // ChannelArray is a single request-response op (GPR-like), so the frame
    // SID is not re-validated; INIT keeps the frame's channel.
    let sid = if subcmd & QosFlags::INIT != 0 {
        sid
    } else {
        data_phase_owner_sid(channels, ioid, sid, false)?.unwrap_or(sid)
    };

    let ch = match channels.get_mut(&sid) {
        Some(c) => c,
        None => {
            send_op_error(
                tx,
                OpKind::Array,
                ioid,
                subcmd,
                Status::error("unknown channel sid"),
                order,
            )
            .await?;
            return Ok(());
        }
    };
    ch.stat.add_op_rx(frame);
    let chan_tx = ChannelTx::new(tx.clone(), ch.stat.clone());

    // Dispatch to the CREATE_CHANNEL-bound owner, not the registry.
    let source = ch.source.clone();
    let source = &source;
    let pv_name = ch.name.clone();

    if subcmd & QosFlags::INIT != 0 {
        if dup_ioid {
            return Err(PvaError::Decode(format!(
                "duplicate ARRAY INIT on live IOID {ioid}"
            )));
        }
        if ch.ops.len() >= config.max_ops_per_channel {
            send_chan_op_error(
                &chan_tx,
                OpKind::Array,
                ioid,
                subcmd,
                Status::error("max ops per channel exceeded"),
                order,
            )
            .await?;
            return Ok(());
        }
        // Decode the pvRequest selecting the array field, through the same
        // structured boundary as GET/PUT/PROCESS INIT. A malformed
        // descriptor, or a non-null descriptor that needs value bytes but
        // ends before them, is a connection-fatal peer wire fault (pvxs
        // `from_wire_type_value`); only the all-empty-structs default
        // selector (and the NULL `0xFF` descriptor) legitimately has a
        // 0-byte value body.
        let (_req_desc, req_value) =
            match decode_init_pv_request(&mut cur, inbound_order, decode_cache) {
                Ok(v) => v,
                Err(e) => {
                    return Err(PvaError::Decode(format!("ARRAY INIT pvRequest: {e}")));
                }
            };
        let ctx = crate::server_native::source::ChannelContext {
            peer,
            account: cred.account.clone(),
            method: cred.method.clone(),
            host: cred.host.clone(),
            authority: cred.authority.clone(),
            roles: cred.roles.clone(),
            pv_request: req_value.clone(),
            log: Default::default(),
        };
        // INIT resolves the array field's introspection (or refuses). No
        // access check here — pvAccessCPP gates per sub-op, not at create.
        match source.channel_array_init(&pv_name, ctx).await {
            Ok(array_desc) => {
                let mask = BitSet::all_set(array_desc.total_bits());
                let mut array_op =
                    non_monitor_op_state(Arc::new(array_desc.clone()), OpKind::Array, mask);
                array_op.pv_request = req_value;
                ch.ops.insert(ioid, array_op);

                let mut payload = Vec::new();
                payload.put_u32(ioid, order);
                payload.put_u8(subcmd);
                Status::ok().write_into(order, &mut payload);
                // INIT reply carries the array introspection
                // (pvAccessCPP `cachedSerialize(_pvArray->getArray())`).
                if config.emit_type_cache {
                    encode_type_desc_cached(&array_desc, order, encode_cache, &mut payload);
                } else {
                    encode_type_desc(&array_desc, order, &mut payload);
                }
                let h = PvaHeader::application(
                    true,
                    order,
                    Command::Array.code(),
                    payload.len() as u32,
                );
                let mut buf = Vec::new();
                h.write_into(&mut buf);
                buf.extend_from_slice(&payload);
                let _ = chan_tx.send(buf).await;
            }
            Err(e) => {
                // Not supported / resolution failure: reply with the error
                // Status on the INIT frame (pvAccessCPP `send`: a creation
                // error still answers QOS_INIT). No op registered.
                send_chan_op_error(
                    &chan_tx,
                    OpKind::Array,
                    ioid,
                    subcmd,
                    e.wire_status(),
                    order,
                )
                .await?;
            }
        }
        return Ok(());
    }

    // Data phase. Bind to the INIT-registered op (kind must match).
    let init_pv_request = match ch.ops.get(&ioid) {
        None => {
            // No op registered for this IOID at the data phase. pvAccessCPP —
            // the only ChannelArray server reference (pvxs has no ARRAY
            // handler) — answers with a CMD_ARRAY error frame (badIOIDStatus,
            // responseHandlers.cpp:2157), NOT a silent drop, so a client
            // awaiting the sub-op callback does not block until timeout.
            send_chan_op_error(
                &chan_tx,
                OpKind::Array,
                ioid,
                subcmd,
                Status::error("bad request id"),
                order,
            )
            .await?;
            return Ok(());
        }
        Some(o) => {
            if o.kind != OpKind::Array {
                return Err(PvaError::Decode(format!(
                    "ARRAY data-phase frame for IOID {ioid} initialised as {:?} \
                     (pvxs serverget.cpp:421-436 protocol error)",
                    o.kind
                )));
            }
            o.pv_request.clone()
        }
    };
    // Array introspection bound at INIT — drives the put-value decode and
    // the get-value encode.
    let array_desc = ch
        .ops
        .get(&ioid)
        .map(|o| o.intro.clone())
        .expect("op present");

    // Decode the sub-op + its windowing params from the frame body now,
    // before the borrow of `cur` ends and the exec task is spawned. A
    // truncated size / value is a connection-fatal wire fault.
    let sub_op = if subcmd & QosFlags::GET != 0 {
        let offset = read_array_size(&mut cur, inbound_order, "getArray offset")?;
        let count = read_array_size(&mut cur, inbound_order, "getArray count")?;
        let stride = read_array_size(&mut cur, inbound_order, "getArray stride")?;
        ChannelArraySubOp::Get {
            offset,
            count,
            stride,
        }
    } else if subcmd & QosFlags::GET_PUT != 0 {
        let length = read_array_size(&mut cur, inbound_order, "setLength length")?;
        ChannelArraySubOp::SetLength { length }
    } else if subcmd & QosFlags::PROCESS != 0 {
        ChannelArraySubOp::GetLength
    } else {
        let offset = read_array_size(&mut cur, inbound_order, "putArray offset")?;
        let stride = read_array_size(&mut cur, inbound_order, "putArray stride")?;
        let value = decode_pv_field_cached(&array_desc, &mut cur, inbound_order, decode_cache)
            .map_err(|e| PvaError::Decode(format!("ARRAY putArray value: {e}")))?;
        ChannelArraySubOp::Put {
            offset,
            stride,
            value,
        }
    };

    let ctx = crate::server_native::source::ChannelContext {
        peer,
        account: cred.account.clone(),
        method: cred.method.clone(),
        host: cred.host.clone(),
        authority: cred.authority.clone(),
        roles: cred.roles.clone(),
        // INIT pvRequest re-supplied so the source knows the bound field.
        pv_request: init_pv_request,
        log: Default::default(),
    };
    let src = source.clone();
    let tx_clone = chan_tx.clone();
    // Run this sub-op only when the op is `Idle`. A second sub-op arriving
    // while one is in flight is a protocol error: pvAccessCPP `startRequest`
    // returns false and the server answers with a CMD_ARRAY error frame
    // (otherRequestPendingStatus, responseHandlers.cpp:2164) rather than
    // ignoring it. The in-flight op is left running, not aborted.
    let op_id = match begin_exec(ch, ioid) {
        Some(id) => id,
        None => {
            debug!(ioid, "ARRAY sub-op rejected: op already executing");
            send_chan_op_error(
                &chan_tx,
                OpKind::Array,
                ioid,
                subcmd,
                Status::error("other request pending"),
                order,
            )
            .await?;
            return Ok(());
        }
    };
    let exec_fin = ExecFinished {
        sid,
        ioid,
        op_id,
        success: false,
    };
    let exec_fin_tx_task = exec_fin_tx.clone();
    let abort = poll_inline_or_spawn(async move {
        let _exec_fin_guard = ExecFinishGuard {
            tx: exec_fin_tx_task,
            fin: exec_fin,
        };
        let checked = src
            .access_gate()
            .check_with_roles(
                &pv_name,
                &ctx.host,
                &ctx.account,
                &ctx.roles,
                &ctx.method,
                &ctx.authority,
            )
            .await;
        // A panic in the (user / gateway) source handler becomes an error
        // reply instead of a skipped reply.
        let op_log = ctx.log.clone();
        let result: Result<ChannelArrayReply, OpError> = match sub_op {
            ChannelArraySubOp::Get {
                offset,
                count,
                stride,
            } => catch_handler_panic(src.channel_array_get(checked, offset, count, stride, ctx))
                .await
                .map_err(OpError::failed)
                .and_then(|r| r)
                .map(ChannelArrayReply::Value),
            ChannelArraySubOp::Put {
                offset,
                stride,
                value,
            } => catch_handler_panic(src.channel_array_put(checked, offset, stride, value, ctx))
                .await
                .map_err(OpError::failed)
                .and_then(|r| r)
                .map(|()| ChannelArrayReply::Empty),
            ChannelArraySubOp::SetLength { length } => {
                catch_handler_panic(src.channel_array_set_length(checked, length, ctx))
                    .await
                    .map_err(OpError::failed)
                    .and_then(|r| r)
                    .map(|()| ChannelArrayReply::Empty)
            }
            ChannelArraySubOp::GetLength => {
                catch_handler_panic(src.channel_array_get_length(checked, ctx))
                    .await
                    .map_err(OpError::failed)
                    .and_then(|r| r)
                    .map(ChannelArrayReply::Length)
            }
        };
        flush_remote_log(&op_log, ioid, order, &tx_clone).await;

        let mut payload = Vec::new();
        payload.put_u32(ioid, order);
        payload.put_u8(subcmd);
        match result {
            Ok(reply) => {
                Status::ok().write_into(order, &mut payload);
                match reply {
                    ChannelArrayReply::Empty => {}
                    ChannelArrayReply::Value(v) => {
                        encode_pv_field(&v, &array_desc, order, &mut payload);
                    }
                    ChannelArrayReply::Length(n) => {
                        encode_size_into(n, order, &mut payload);
                    }
                }
            }
            Err(e) => e.wire_status().write_into(order, &mut payload),
        }
        let h = PvaHeader::application(true, order, Command::Array.code(), payload.len() as u32);
        let mut buf = Vec::new();
        h.write_into(&mut buf);
        buf.extend_from_slice(&payload);
        let _ = tx_clone.send(buf).await;
    });
    finish_exec_data_task(ch, ioid, subcmd, abort);
    Ok(())
}

/// Read one PVA `Size` from a ChannelArray data-phase body, mapping the
/// null marker / truncation to a connection-fatal decode error (a
/// ChannelArray offset/count/stride/length is never the nullable form).
fn read_array_size(
    cur: &mut std::io::Cursor<&[u8]>,
    order: ByteOrder,
    what: &str,
) -> PvaResult<u32> {
    match crate::proto::decode_size(cur, order) {
        Ok(Some(n)) => Ok(n),
        Ok(None) => Err(PvaError::Decode(format!("ARRAY {what}: null size marker"))),
        Err(e) => Err(PvaError::Decode(format!("ARRAY {what}: {e}"))),
    }
}

async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    rx_buf: &mut Vec<u8>,
    op_timeout: Duration,
    max_msg_size: Option<usize>,
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
        // Peek the header length once we have 8 bytes — if `max_msg_size` is
        // set and the peer claimed more, refuse before growing rx_buf any
        // further, so the IOC never spends the memory or the bandwidth on a
        // message it has already decided to reject. `None` is the explicit
        // opt-out to pvxs's uncapped RX. Even unbounded, the read stays
        // incremental (4 KiB chunks), `op_timeout`-deadlined, and now
        // fallible (`peer_buf::try_extend`), so a stalled, oversized, or
        // heap-exhausting peer costs this connection and no other.
        if let Some(cap) = max_msg_size {
            if rx_buf.len() >= PvaHeader::SIZE {
                if let Ok(hdr) = PvaHeader::decode(&mut std::io::Cursor::new(&rx_buf[..])) {
                    if !hdr.flags.is_control() && hdr.payload_length as usize > cap {
                        return Err(PvaError::Protocol(format!(
                            "inbound payload {} exceeds max_message_size {}",
                            hdr.payload_length, cap
                        )));
                    }
                }
            }
        }
        let mut chunk = [0u8; 4096];
        let n = match epics_base_rs::runtime::task::timeout(op_timeout, reader.read(&mut chunk))
            .await
        {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(PvaError::Io(e)),
            Err(_) => return Err(PvaError::Timeout),
        };
        if n == 0 {
            return Err(PvaError::Protocol("client closed".into()));
        }
        try_extend(rx_buf, &chunk[..n], "the connection receive buffer")?;
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

/// spawn-based CREATE_CHANNEL handler. For each (cid, name)
/// pair in the frame, cap-exceeded pairs are rejected synchronously
/// (no source call needed); all others spawn a background resolver
/// task that calls `has_pv` + `get_introspection` and sends the result
/// through `cc_tx` back to the read loop, which inserts the channel
/// and emits the wire response in FIFO order.
#[allow(clippy::too_many_arguments)]
async fn handle_create_channel(
    source: &DynSource,
    frame: &Frame,
    tx: &SrvTx,
    channels: &HashMap<u32, ChannelState>,
    order: ByteOrder,
    max_channels_per_connection: usize,
    peer: SocketAddr,
    cred: &ClientCredentials,
    cc_tx: &CcTx,
    pending_channel_spawns: &mut usize,
) -> PvaResult<()> {
    // Inbound payload decodes with the frame's own header order (pvxs
    // latches `peerBE` per received message, conn.cpp:195-198); `order`
    // (config) is used only for outbound reply frames.
    let inbound_order = frame.order();
    let mut cur = frame.cursor();
    // pvxs `serverchan.cpp:269-358`: a single CREATE_CHANNEL frame
    // can carry `count` (cid, name) pairs and the server must emit
    // one CREATE_CHANNEL response frame per pair, in arrival order.
    let count = cur
        .get_u16(inbound_order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;

    // Collect entries to resolve asynchronously. We allocate SIDs
    // up-front so the cap is known before spawning, then spawn ONE
    // task that resolves names sequentially — this guarantees responses
    // arrive in arrival order (pvxs serverchan.cpp parity).
    let mut batch: Vec<(u32, u32, String)> = Vec::new(); // (cid, sid, name)

    for _ in 0..count {
        // truncated CID / malformed string is a protocol-
        // fatal decode error. pvxs `serverchan.cpp:364-368`.
        let cid = cur
            .get_u32(inbound_order)
            .map_err(|e| PvaError::Decode(format!("CREATE_CHANNEL cid: {e}")))?;
        let name = match crate::proto::decode_string(&mut cur, inbound_order)
            .map_err(|e| PvaError::Decode(format!("CREATE_CHANNEL name: {e}")))?
        {
            Some(s) => s,
            None => break,
        };
        if name.is_empty() {
            break;
        }

        // per-channel cap check: open channels + in-flight spawns
        // from previous frames + already-batched names in this frame.
        if channels.len() + *pending_channel_spawns + batch.len() >= max_channels_per_connection {
            warn!(
                ?peer,
                pv = %name,
                "rejecting CREATE_CHANNEL: per-connection limit reached"
            );
            let mut payload = Vec::new();
            payload.put_u32(cid, order);
            // CREATE_CHANNEL failure sid must be the no-channel sentinel
            // 0xFFFFFFFF (pvxs serverchan.cpp:349, sid=-1), not 0.
            payload.put_u32(CREATE_CHANNEL_NO_SID, order);
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

        batch.push((cid, alloc_sid(), name));
    }

    // Spawn ONE task per frame that resolves names in order and streams
    // completions back via cc_tx. Per-name separate spawns would race
    // and reorder responses; sequential resolution inside one task is
    // both correct and sufficient for any well-behaved source.
    if !batch.is_empty() {
        *pending_channel_spawns += batch.len();
        let src = source.clone();
        let cc = cc_tx.clone();
        // resolve existence + introspection under the
        // downstream connection's identity so a gateway opens upstream
        // state under THIS peer's credentials, not the shared identity.
        // pvxs builds `ServerChannelControl` with `conn->cred`
        // (`serverchan.cpp:62`). `pv_request` is `None` — CREATE_CHANNEL
        // carries no per-op pvRequest.
        //
        // Snapshot the credential in force NOW, before the resolver runs:
        // the channel is created under this identity and its lifecycle
        // callbacks must use it even if the connection re-authenticates to a
        // different identity while the resolver is still in flight. The
        // snapshot rides back in each completion and is stored on the channel.
        // The resolver's `ChannelContext` is built from the same snapshot, so
        // resolution and the open callback agree.
        let open_cred = cred.clone();
        let conn_ctx = channel_lifecycle_ctx(peer, &open_cred);
        epics_base_rs::runtime::task::spawn(async move {
            for (cid, sid, nm) in batch {
                let resolved = if src.has_pv_checked(&nm, conn_ctx.clone()).await {
                    // Bind the owner that accepted this channel so every
                    // later op dispatches there, never re-resolving the
                    // registry (pvxs serverchan.cpp:70-112). A leaf source
                    // is its own owner (`resolve_owner` returns `None`);
                    // a composite returns the matched inner.
                    let owner = match src.resolve_owner(&nm, conn_ctx.clone()).await {
                        Some(inner) => inner,
                        None => src.clone(),
                    };
                    // Negotiate the descriptor through the bound owner, so
                    // it matches the source that will serve the operations.
                    let intro = owner
                        .get_introspection_checked(&nm, conn_ctx.clone())
                        .await
                        .map(Arc::new);
                    // Capture the owner's per-channel report info once at
                    // admission — pvxs lets a Source stash a `ReportInfo`
                    // on the channel control during onCreate
                    // (`source.h:192`), surfaced later in `Report::Channel`.
                    let report_info = owner.channel_report_info(&nm, conn_ctx.clone()).await;
                    Some(ResolvedChannel {
                        intro,
                        owner,
                        report_info,
                    })
                } else {
                    None
                };
                let _ = cc
                    .send(CreateChannelCompletion {
                        cid,
                        sid,
                        name: nm,
                        open_cred: open_cred.clone(),
                        resolved,
                    })
                    .await;
            }
        });
    }
    Ok(())
}

/// Build the connection-scoped [`ChannelContext`](crate::server_native::ChannelContext) for a channel
/// *lifecycle* edge (open/close). pvAccess carries no per-op `pvRequest`
/// on these edges, so `pv_request` is `None` — the same shape
/// CREATE_CHANNEL uses to resolve the owner (serverchan.cpp:62, the
/// channel is built from `conn->cred`).
fn channel_lifecycle_ctx(
    peer: SocketAddr,
    cred: &ClientCredentials,
) -> crate::server_native::source::ChannelContext {
    crate::server_native::source::ChannelContext {
        peer,
        account: cred.account.clone(),
        method: cred.method.clone(),
        host: cred.host.clone(),
        authority: cred.authority.clone(),
        roles: cred.roles.clone(),
        pv_request: None,
        log: Default::default(),
    }
}

/// Tear a single channel down and deliver its `onClose` to the bound
/// source, in pvxs `ServerChan::cleanup` order: drop the per-op state
/// first — aborting any monitor subscriber tasks and releasing
/// source-side subscriptions — then notify the source exactly once
/// (`serverchan.cpp:43-60`, `:115-127`). `peer` plus the channel's stored
/// `open_cred` reconstruct the identity the channel was *created* under —
/// pvxs delivers `onClose` with the channel-control credential
/// (`serverchan.cpp:62`), so a re-auth between open and teardown must not
/// change the close identity.
fn close_channel(ch: ChannelState, peer: SocketAddr) {
    let ChannelState {
        name,
        source,
        open_cred,
        ops,
        ..
    } = ch;
    drop(ops);
    let ctx = channel_lifecycle_ctx(peer, &open_cred);
    source.notify_channel_close(&name, &ctx);
}

/// Connection-scoped context for a channel teardown that emits a
/// DESTROY_CHANNEL frame: the writer handle, the outbound byte order, the
/// peer's address, and the per-peer report registry. Bundling these four
/// keeps the teardown helpers ([`finalize_channel_destroy`],
/// [`handle_destroy_channel`], [`invalidate_named_channels`]) within the
/// argument-count budget.
///
/// It deliberately carries NO credential: the source `onClose` lifecycle
/// callback is delivered with the channel's own stored open-time credential
/// (`ChannelState::open_cred`), pinned at CREATE_CHANNEL, not the
/// connection's current identity — which a re-auth can reassign after the
/// channel is open.
/// `close_channel` reconstructs the lifecycle [`ChannelContext`](crate::server_native::ChannelContext) from that
/// stored snapshot and `ctx.peer`.
struct ChannelTeardownCtx<'a> {
    tx: &'a SrvTx,
    order: ByteOrder,
    peer: SocketAddr,
    peer_entry: &'a Arc<crate::server_native::peers::PeerEntry>,
}

/// Why a channel is being torn down with a DESTROY_CHANNEL frame. pvxs
/// attributes the 16-byte reply to the report differently for the two
/// causes, so the single teardown owner must know which one it serves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DestroyCause {
    /// Server-initiated unsolicited destroy: PVA gateway operator
    /// `:drop`/`:flush`, the read loop's invalidation arm. pvxs
    /// `ServerChannelControl::close()` sends the reply and charges BOTH
    /// the connection AND the channel `statTx += 16u`
    /// (serverchan.cpp:151-152), then runs `cleanup()` — so the report
    /// can show the live channel with the 16-byte reply attributed.
    ServerInitiated,
    /// Client-initiated `DESTROY_CHANNEL` (cmd 8): the peer asked us to
    /// drop the channel. pvxs `ServerConn::handle_DESTROY_CHANNEL()`
    /// erases the channel from `chanBySID` and cleans it up FIRST, then
    /// sends the 16-byte reply charging only the connection `statTx`
    /// with the explicit note "don't bother to increment for channel"
    /// (serverchan.cpp:404-411). The per-channel report entry is gone
    /// before the reply, so no report attributes the reply to a channel.
    ClientInitiated,
}

/// Single owner of a channel teardown that notifies the peer with a
/// DESTROY_CHANNEL frame. EVERY teardown path that emits DESTROY_CHANNEL —
/// the client-initiated [`handle_destroy_channel`] and the server-initiated
/// out-of-band invalidation (PVA gateway operator `:drop`/`:flush`, the
/// read loop's invalidation arm) — funnels through here, so the ordering
/// and the report bookkeeping cannot drift between callers. In one place it:
///
/// 1. removes `sid` from the connection table (dropping every `OpState`,
///    which aborts monitor subscriber tasks and releases source-side
///    subscriptions),
/// 2. runs [`close_channel`] (op teardown → bound source `onClose`, pvxs
///    `ServerChan::cleanup` serverchan.cpp:43-60),
/// 3. emits the DESTROY_CHANNEL frame (`sid + cid`), and
/// 4. drops the `PeerEntry` report entry (`channel_closed`, pvxs removes
///    from `conn->chanBySID`).
///
/// Steps 3 and 4 differ by `cause` to match pvxs report accounting (see
/// [`DestroyCause`]): the server-initiated path charges the reply to the
/// channel's `statTx` then drops the report entry (pvxs `close()`); the
/// client-initiated path drops the report entry FIRST and charges no
/// channel `statTx` (pvxs `handle_DESTROY_CHANNEL()`). The
/// connection-level `bytes_out` counter is charged by the writer task for
/// the frame in BOTH cases, so only the per-channel attribution varies.
///
/// Returns `true` when a channel was actually present and torn down,
/// `false` for an unknown SID (no-op, no reply). `ctx.order` is the
/// outbound reply order.
async fn finalize_channel_destroy(
    sid: u32,
    cid: u32,
    cause: DestroyCause,
    channels: &mut HashMap<u32, ChannelState>,
    ctx: &ChannelTeardownCtx<'_>,
) -> bool {
    // Removing the channel drops every OpState in `ops`, which drops each
    // `monitor_abort: Option<Arc<AbortOnDrop>>` and cancels the associated
    // subscriber task — preventing orphaned spawns from holding the
    // source's broadcast subscription. `close_channel` does that op
    // teardown first, then notifies the bound source's `onClose`.
    let Some(ch) = channels.remove(&sid) else {
        return false;
    };
    // Capture the channel's stat before `close_channel` consumes the
    // state, so the server-initiated path can charge the reply below.
    let stat = ch.stat.clone();
    // `close_channel` delivers `onClose` with the channel's stored
    // open-time credential, not `ctx.cred` (the connection's current
    // identity) — pinned.
    close_channel(ch, ctx.peer);

    // Client-initiated DESTROY: pvxs `handle_DESTROY_CHANNEL()` erases the
    // channel from `chanBySID` BEFORE sending the reply, so drop the report
    // entry now. With no channel `statTx` charge below, a concurrent report
    // never shows this channel carrying the destroy reply.
    if cause == DestroyCause::ClientInitiated {
        ctx.peer_entry.channel_closed(sid);
    }

    let mut payload = Vec::new();
    payload.put_u32(sid, ctx.order);
    payload.put_u32(cid, ctx.order);
    let h = PvaHeader::application(
        true,
        ctx.order,
        Command::DestroyChannel.code(),
        payload.len() as u32,
    );
    let mut buf = Vec::new();
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    // Server-initiated close charges the reply to the channel's `statTx`
    // (pvxs `ch->statTx += 16u`, serverchan.cpp:152); the client-initiated
    // path charges only the connection counter (the writer task's
    // `bytes_out`), matching pvxs serverchan.cpp:409-410.
    if cause == DestroyCause::ServerInitiated {
        stat.add_tx(buf.len());
    }
    let _ = ctx.tx.send(buf).await;

    // Server-initiated close drops the report entry AFTER charging the
    // reply (pvxs `cleanup()` runs after `ch->statTx += 16u`). Folded into
    // this finalizer so no teardown caller can forget it — a no-op for a
    // SID never registered (`channel_closed` is gated on a successful map
    // removal). The client-initiated path already dropped it above.
    if cause == DestroyCause::ServerInitiated {
        ctx.peer_entry.channel_closed(sid);
    }
    true
}

/// Force-disconnect every channel this connection serves under
/// `pv`, each through the single teardown owner [`finalize_channel_destroy`]
/// (server-initiated DESTROY_CHANNEL). Called by the read loop's
/// channel-invalidation arm for every PV name a source publishes on an
/// operator `:drop`/`:flush`. Collects matching SIDs first (immutable
/// borrow) so the per-victim mutable teardown does not alias the map.
/// Returns the number of channels torn down (0 when this connection holds
/// none under that name — the common case, since the invalidation is
/// server-wide). A channel hosts every op under one name, so this ends that
/// name's GET/PUT/MONITOR together, matching pva2pva's per-channel destroy.
async fn invalidate_named_channels(
    pv: &str,
    channels: &mut HashMap<u32, ChannelState>,
    ctx: &ChannelTeardownCtx<'_>,
) -> usize {
    let victims: Vec<(u32, u32)> = channels
        .iter()
        .filter(|(_, ch)| ch.name == pv)
        .map(|(&sid, ch)| (sid, ch.cid))
        .collect();
    let mut torn_down = 0;
    for (sid, cid) in victims {
        if finalize_channel_destroy(sid, cid, DestroyCause::ServerInitiated, channels, ctx).await {
            torn_down += 1;
        }
    }
    torn_down
}

/// Client-initiated DESTROY_CHANNEL (cmd 8). Validates the SID against the
/// connection table, then delegates the teardown to the single owner
/// [`finalize_channel_destroy`].
async fn handle_destroy_channel(
    frame: &Frame,
    channels: &mut HashMap<u32, ChannelState>,
    ctx: &ChannelTeardownCtx<'_>,
) -> PvaResult<()> {
    // Inbound payload decodes with the frame's own header order (pvxs
    // latches `peerBE` per received message, conn.cpp:195-198); `ctx.order`
    // (config) is used only for the outbound reply below.
    let inbound_order = frame.order();
    let mut cur = frame.cursor();
    let sid = cur
        .get_u32(inbound_order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let cid = cur
        .get_u32(inbound_order)
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
    finalize_channel_destroy(sid, cid, DestroyCause::ClientInitiated, channels, ctx).await;
    Ok(())
}

/// Handle CANCEL_REQUEST (cmd 21). pvxs serverconn.cpp:262 — moves the op
/// from Executing back to Idle without freeing it; the underlying
/// `MonitorOp` (and the source's onSubscribe state) stays alive so a
/// later START restores Executing without re-issuing the subscription.
///
/// Cancel-vs-destroy refactor: previously the Rust handler
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
) -> PvaResult<()> {
    // Decode with the frame's own header order (pvxs conn.cpp:195-198).
    let order = frame.order();
    let mut cur = frame.cursor();
    // pvxs `serverconn.cpp:262-270` throws on truncated
    // CANCEL_REQUEST (`if(!M.good()) throw ...`), which the conn
    // loop turns into a connection reset. Pre-fix Rust silently
    // returned. Mirror pvxs — bubble as a fatal decode error.
    let sid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(format!("CANCEL_REQUEST sid: {e}")))?;
    let ioid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(format!("CANCEL_REQUEST ioid: {e}")))?;
    // pvxs `serverconn.cpp:262-291` keys CANCEL on the connection-wide
    // `opByIOID`, then rejects it when the located op's channel SID does not
    // match the supplied SID ("Cancel inconsistent Op"). Locate by IOID across
    // the connection so the SID is validated against the op's real owner.
    let osid = match op_owner_sid(channels, ioid) {
        Some(s) if s == sid => s,
        Some(_) => {
            debug!(sid, ioid, "CANCEL_REQUEST with inconsistent SID: dropping");
            return Ok(());
        }
        None => {
            debug!(sid, ioid, "CANCEL_REQUEST for non-existent op: dropping");
            return Ok(());
        }
    };
    if let Some(ch) = channels.get_mut(&osid)
        && let Some(op) = ch.ops.get_mut(&ioid)
    {
        // pvxs `serverconn.cpp:262-295` applies CANCEL_REQUEST to EVERY
        // executing op kind by flipping `ServerOp::state` to `Idle`. The
        // transition differs by kind, so split here:
        if op.kind == OpKind::Monitor {
            // MONITOR has a long-lived subscriber: suspend it WITHOUT
            // aborting the task, so the next START flips it back to
            // Executing. Route the Executing→Idle edge through the op's
            // single start-control owner so `notify_monitor_start(false)`
            // fires once (no-op if already paused / never started);
            // DESTROY's terminal stop comes from `Drop`.
            op.monitor_paused
                .store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(ctl) = &op.monitor_start_ctl {
                ctl.set(false);
            }
        } else if op.exec_state == ExecState::Executing {
            // Non-monitor (GET/PUT/RPC/PUT_GET/PROCESS) in flight. pvxs
            // sets the op `Idle`, after which a late reply is dropped
            // because the op is no longer Executing (`serverget.cpp:37-49`
            // names remote Cancel as the cause) and a subsequent EXEC is
            // accepted (`serverget.cpp:511-514`). Close all three here:
            //   1. return the op to `Idle`;
            //   2. mint a fresh op-instance id so the in-flight task's
            //      terminal `ExecFinished` is ignored by the
            //      `apply_exec_finish` ABA guard (it must not later flip a
            //      re-EXEC'd op or remove it on a stale `last_request`);
            //   3. preserve `last_request` — pvxs `CANCEL_REQUEST`
            //      (`serverconn.cpp:262-289`) sets `state = Idle` but never
            //      clears `ServerGPR::lastRequest`; the sticky destroy marker
            //      survives, and `if(!op->lastRequest) op->lastRequest = ...`
            //      keeps it true on the next EXEC (`serverget.cpp:470-471`) so
            //      that EXEC's `doReply` cleans the op up after replying
            //      (`serverget.cpp:111-114`). The fresh op-instance id from
            //      step 2 already neutralizes the canceled task's late
            //      completion, so the sticky marker only takes effect on a
            //      genuine re-EXEC's reply — clearing it here would leak an op
            //      pvxs would have released;
            //   4. drop the abort guard — aborting the spawned task both
            //      prevents its late reply AND drops the in-flight source
            //      future, which is the Rust structured-concurrency
            //      equivalent of pvxs `ExecOp::onCancel`
            //      (`serverget.cpp:266-321`): cancelling the task cancels
            //      the source call, with no separate source-facing hook.
            op.exec_state = ExecState::Idle;
            op.monitor_op_id = next_op_id();
            op.data_task_abort = None;
        }
    }
    Ok(())
}

/// Handle MESSAGE (cmd 18). pvxs serverconn.cpp:323 — clients send
/// log messages tagged with severity (Info/Warning/Error/Fatal) bound
/// to an operation IOID. We surface them through the `tracing` crate
/// at the matching level.
///
/// pvxs (`serverconn.cpp:338-354`) looks the IOID up in the
/// connection-wide `opByIOID` FIRST: a MESSAGE for an IOID no operation
/// owns is logged only at debug and dropped — it never reaches the
/// warning/error severity path. Only a live IOID gets the severity
/// mapping, with the owning channel name in the log line. Without this
/// gate a peer could emit warning/error-level server logs for an
/// arbitrary IOID it never opened.
fn handle_message(
    frame: &Frame,
    channels: &HashMap<u32, ChannelState>,
    peer: &SocketAddr,
) -> PvaResult<()> {
    // Decode with the frame's own header order (pvxs conn.cpp:195-198).
    let order = frame.order();
    let mut cur = frame.cursor();
    // pvxs `serverconn.cpp:323-336` throws on malformed
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
    // IOID lookup gate (pvxs serverconn.cpp:338-342): absent → debug
    // only, no severity escalation.
    let channel = op_owner_sid(channels, ioid).and_then(|sid| channels.get(&sid));
    let Some(channel) = channel else {
        debug!(
            ?peer,
            ioid, mtype, message = %msg,
            "client MESSAGE for unknown IOID: dropping (no owning operation)"
        );
        return Ok(());
    };
    let pv = channel.name.as_str();
    // pvxs `mtype2level` (pvaproto.h:704-712, serverconn.cpp:346-351):
    // 0 -> Info, 1 -> Warn, 2 -> Err, and every other value (including the
    // protocol's Fatal=3 and any unknown type) -> Crit. The tracing stack
    // has no Crit level, so both Err and Crit map to its highest level,
    // error!. A live-IOID MESSAGE of any type is surfaced at its pvxs
    // severity; unknown/Fatal types escalate to error!, never hidden at
    // debug. The unknown-IOID gate above stays debug-only.
    match mtype {
        0 => info!(?peer, ioid, pv, message = %msg, "client message"),
        1 => warn!(?peer, ioid, pv, message = %msg, "client message"),
        _ => error!(?peer, ioid, pv, mtype, message = %msg, "client message"),
    }
    Ok(())
}

fn handle_destroy_request(
    frame: &Frame,
    channels: &mut HashMap<u32, ChannelState>,
) -> PvaResult<()> {
    // Decode with the frame's own header order (pvxs conn.cpp:195-198).
    let order = frame.order();
    let mut cur = frame.cursor();
    // pvxs `serverconn.cpp:297-305` throws on malformed
    // DESTROY_REQUEST. Pre-fix Rust silently returned.
    let sid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(format!("DESTROY_REQUEST sid: {e}")))?;
    let ioid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(format!("DESTROY_REQUEST ioid: {e}")))?;
    // pvxs `serverconn.cpp:295-319` keys DESTROY on the connection-wide
    // `opByIOID` and erases + `cleanup()`s the op whenever the IOID is found,
    // even if the supplied SID does not match the op's channel (the
    // channel-local erase merely warns). Locating the op by IOID across the
    // connection — rather than only inside the frame's SID — destroys the op a
    // mis-addressed DESTROY would otherwise leak.
    let _ = sid;
    if let Some(osid) = op_owner_sid(channels, ioid)
        && let Some(ch) = channels.get_mut(&osid)
    {
        // Removing the op drops `monitor_abort: Option<Arc<AbortOnDrop>>`.
        // Once the last clone is dropped, the subscriber task aborts.
        ch.ops.remove(&ioid);
    }
    Ok(())
}

/// handle `Command::Search` arriving on an established
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
    peer: SocketAddr,
) -> PvaResult<()> {
    // Rebuild the raw frame bytes so the UDP parser sees the same
    // shape (header + payload). `parse_search_request` reads from
    // the header inwards.
    let mut raw: Vec<u8> = Vec::with_capacity(PvaHeader::SIZE + frame.payload.len());
    frame.header.write_into(&mut raw);
    raw.extend_from_slice(&frame.payload);

    let Some(req) = super::search::parse_search_request(&raw) else {
        // The command framing already classified this frame as a SEARCH,
        // so `parse_search_request` returning `None` here means the body
        // failed to decode (truncated, bad size prefix, missing channel
        // name) — not "this isn't a SEARCH". On an established TCP circuit
        // that is a protocol fault: pvxs decodes the body, checks
        // `!M.good()` and throws "TCP Search decode error"
        // (serverchan.cpp:209-210), which the connection dispatcher treats
        // as a circuit fault and tears the connection down. We surface the
        // same fault as a connection-level decode error so the read loop
        // closes the circuit rather than silently skipping the frame and
        // continuing to serve a peer that already corrupted the stream.
        // (UDP keeps the datagram-drop ignore path: a bad datagram there
        // is not a stream fault.)
        return Err(PvaError::Decode(
            "TCP SEARCH decode error (pvxs serverchan.cpp:209-210)".into(),
        ));
    };

    // Default protocol on TCP is "tcp" (or "tls" when TLS is in use); it
    // names the transport advertised back in the SEARCH_RESPONSE.
    let protocol: &'static str = if config.tls.is_some() { "tls" } else { "tcp" };
    // Port advertised for `protocol`: pvxs returns `tls_port` for a
    // protoTLS reply and `tcp_port` otherwise (server.cpp:849-857), so a
    // TLS server steers the client to its dedicated TLS listener. The
    // runtime stamps `config.tls_port` to the bound TLS port at start().
    let advertised_port = if config.tls.is_some() {
        config.tls_port
    } else {
        config.tcp_port
    };
    // Match WITHOUT the UDP protocol gate. pvxs `handle_SEARCH` parses the
    // SEARCH protocol strings into `foundtcp` but never consults it before
    // calling every source's `onSearch` (serverchan.cpp:184-244): on an
    // established circuit the transport was already negotiated at connect
    // time, so a SEARCH payload's protocol list (e.g. `["tls"]` arriving on
    // a plaintext TCP circuit) must NOT suppress matches. The byte-exact
    // protocol gate stays on the v4/v6 UDP responders (a broadcast SEARCH
    // must not pull `found=1` from a server that does not speak the
    // requested transport). pvxs fills `Search::source` from the TCP peer
    // (serverchan.cpp:197-222), so a source can still scope advertisement
    // by the established peer endpoint via `searchable_from`.
    let matched = super::search::matched_cids_for_requester(source, &req, peer).await;
    // pvxs `serverchan.cpp:240-249`: emit the response only when
    // there's a match OR MustReply was set. Skip otherwise to
    // avoid leaking server presence on every probe.
    if !matched.is_empty() || req.must_reply {
        let response = super::search::build_search_response_proto(
            config.guid,
            req.seq,
            advertised_port,
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
    frame: &Frame,
    tx: &SrvTx,
    channels: &mut HashMap<u32, ChannelState>,
    order: ByteOrder,
    // The connection's LIVE outbound byte-order cell (the read loop is its
    // single owner; it re-stores on every mid-stream SET_BYTE_ORDER). The
    // synchronous reply paths use the `order` snapshot, but a spawned MONITOR
    // task outlives this call and must follow a later renegotiation, so it
    // clones this cell and reads it at every frame build (pvxs `conn.cpp:
    // 169-188` re-latches `sendBE`; `servermon.cpp:159,174` reads
    // `conn->sendBE` at monitor send time).
    out_order: &Arc<std::sync::atomic::AtomicBool>,
    kind: OpKind,
    config: &PvaServerConfig,
    encode_cache: &mut EncodeTypeCache,
    // Connection-scope inbound decode cache (pvxs `rxRegistry`, conn.h:23).
    // Threaded so a 0xFD-defined descriptor in the pvRequest/EXEC body of one
    // op resolves a later 0xFE reference on the same connection.
    decode_cache: &mut TypeCache,
    peer: std::net::SocketAddr,
    cred: &ClientCredentials,
    // read-loop owner's MONITOR subscriber-completion sender. The
    // spawned subscriber task installs a `MonitorFinishGuard` cloned from
    // this so its terminal op removal is routed back to the owner. Only the
    // MONITOR branch uses it.
    mon_fin_tx: &mpsc::UnboundedSender<MonitorFinished>,
    // read-loop owner's data-phase-completion sender. Each spawned
    // GET/PUT/RPC data task installs an `ExecFinishGuard` cloned from this so
    // the owner returns the op to `Idle` when the response is sent.
    exec_fin_tx: &mpsc::UnboundedSender<ExecFinished>,
) -> PvaResult<()> {
    // Inbound payload decodes with the frame's own header order (pvxs
    // latches `peerBE` per received message, conn.cpp:195-198); `order`
    // (config) is used only for outbound reply frames. Each data-phase
    // frame re-enters this handler, so `frame.order()` is the order of
    // whichever INIT/EXEC/ACK frame is being decoded right now.
    let inbound_order = frame.order();
    let mut cur = frame.cursor();
    let sid = cur
        .get_u32(inbound_order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let ioid = cur
        .get_u32(inbound_order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let subcmd = cur.get_u8().map_err(|e| PvaError::Decode(e.to_string()))?;

    // Connection-wide IOID uniqueness, evaluated before the per-channel
    // borrow below (pvxs `ServerConn::opByIOID`, serverget.cpp:378-384).
    let dup_ioid = subcmd & 0x08 != 0 && ioid_live_on_conn(channels, ioid);

    // pvxs services a data-phase (non-INIT) frame via the connection-wide
    // `opByIOID` map and acts on `op->chan`, IGNORING the frame SID for
    // GET/PUT/RPC (serverget.cpp:421-423); MONITOR additionally resets the
    // circuit when the frame SID does not own the IOID (servermon.cpp:
    // 610-635). Resolve the owner before the per-channel borrow so a data
    // frame whose IOID is live on another channel is serviced on its real
    // channel instead of being silently dropped. INIT keeps binding to the
    // frame's own channel.
    let sid = if subcmd & 0x08 != 0 {
        sid
    } else {
        data_phase_owner_sid(channels, ioid, sid, kind == OpKind::Monitor)?.unwrap_or(sid)
    };

    let ch = match channels.get_mut(&sid) {
        Some(c) => c,
        None => {
            // unknown SID on INIT must be connection-fatal (pvxs serverget.cpp:378-384
            // calls bev.reset()); on data-phase it should silently drop (pvxs
            // serverget.cpp:423-428 just returns).
            if subcmd & 0x08 != 0 {
                return Err(PvaError::Decode(format!(
                    "INIT on unknown channel SID {sid} (pvxs serverget.cpp:378-384 protocol error)"
                )));
            }
            return Ok(());
        }
    };

    // Attribute this inbound op frame to the channel's report counter as
    // its full framed length (`PvaHeader::SIZE + body`), matching pvxs
    // `chan->statRx += rxlen` where `rxlen = 8u + body` (serverget.cpp:386
    // / servermon.cpp:513). `add_op_rx` owns the `+ header`, so no site
    // can regress to charging the body alone. All per-channel reply/data
    // frames go out through `chan_tx`, the single owner of `statTx`
    // accounting, so the outbound count cannot be skipped at any send site.
    ch.stat.add_op_rx(frame);
    let chan_tx = ChannelTx::new(tx.clone(), ch.stat.clone());

    // Dispatch every operation to the source bound at CREATE_CHANNEL,
    // not the top-level registry. pvxs installs the accepting source's
    // callbacks into the ServerChan (serverchan.cpp:70-112) and a later
    // removeSource never rewrites them (server.cpp:100-112), so a live
    // channel keeps its owner even when the registry changes underneath.
    let source = ch.source.clone();
    let source = &source;

    if subcmd & 0x08 != 0 {
        // duplicate INIT on a live IOID is connection-fatal
        // per pvxs. `serverget.cpp:378-384` and `servermon.cpp:505-511`
        // reset the connection on `op->state != Created`; we model
        // "already created" as `ch.ops.contains_key(&ioid)`. Pre-fix
        // Rust let the insert below silently REPLACE the existing
        // OpState, which could drop a MONITOR subscriber task and
        // redirect later data frames to a different descriptor/mask
        // than the original operation negotiated. pvxs scopes this to the
        // whole connection (`opByIOID`), so a reused IOID on a *different*
        // channel is equally fatal.
        if dup_ioid {
            return Err(PvaError::Decode(format!(
                "duplicate INIT on live IOID {ioid} (pvxs serverget.cpp:378-384 protocol error)"
            )));
        }
        // per-channel concurrent-op cap — refuse fresh INITs
        // once the channel's `ops` map hits the configured ceiling
        // so a malicious peer can't accumulate IOID state forever
        // by sending INIT … INIT … without ever issuing DESTROY.
        if ch.ops.len() >= config.max_ops_per_channel {
            send_chan_op_error(
                &chan_tx,
                kind,
                ioid,
                subcmd,
                Status::error("max ops per channel exceeded"),
                order,
            )
            .await?;
            return Ok(());
        }

        // pvxs `serverget.cpp:182-193` rejects missing
        // prototype for non-RPC operations with "Must provide
        // prototype". Rust's previous fallback turned a source bug
        // (no `get_introspection`) into a successful GET/PUT/MONITOR
        // INIT with a `Variant` descriptor — masking the bug and
        // letting later mismatched-value encoding look valid. RPC
        // can still proceed without a prototype (descriptor-late).
        let intro = match (kind, ch.introspection.clone()) {
            (OpKind::Rpc, Some(d)) => d,
            (OpKind::Rpc, None) => Arc::new(FieldDesc::Variant),
            (_, Some(d)) => d,
            (_, None) => {
                send_chan_op_error(
                    &chan_tx,
                    kind,
                    ioid,
                    subcmd,
                    Status::error("must provide prototype"),
                    order,
                )
                .await?;
                return Ok(());
            }
        };

        // INIT — read pvRequest (`type + full value` per pvxs
        // clientget.cpp:351-352) and translate it to a field mask the
        // emit side will consult.
        //
        // pvxs `serverget.cpp:366-376` and `servermon.cpp:489-502`:
        // `from_wire_type_value(M, rxRegistry, pvRequest)` followed by
        // `if(!M.good()) { bev.reset(); return; }` — a peer wire-decode
        // fault of the pvRequest type/value is connection-fatal and is
        // NOT answered with an op reply. Mirror that: return a
        // connection-fatal `Decode` error (the read loop closes the
        // circuit) instead of a per-op Status that left a malformed-INIT
        // peer free to keep reusing the connection. The EMPTY-MASK case
        // below is different — see the `request_to_mask` arm.
        //
        // The VALUE body is read per pvxs `from_wire_full`. The Rust client's
        // default selectors (and RPC INIT) send a descriptor whose
        // sub-structures are all empty (`pv_request::build`), so the value
        // body is legitimately 0 bytes and decodes fine — no "absent body"
        // exception is needed. A non-null descriptor that needs value bytes
        // but ends before them is the same `!M.good()` bev.reset() wire
        // fault as a malformed descriptor, so it is likewise connection-fatal.
        // A NULL (`0xFF`) descriptor is NOT a fault — see
        // `decode_init_pv_request`.
        let (req_desc, req_value) =
            match decode_init_pv_request(&mut cur, inbound_order, decode_cache) {
                Ok(v) => v,
                Err(e) => {
                    return Err(PvaError::Decode(format!("INIT pvRequest: {e}")));
                }
            };
        // An RPC is never masked. pvxs `serverget.cpp:402` connects it with a
        // default-constructed (falsy) prototype — `ctrl->connect(Value())` —
        // so `ServerGPRConnect::connect`'s `if(prototype)` arm
        // (`serverget.cpp:198-201`) never runs and `request2mask()` is not
        // invoked for CMD_RPC at all. The reply is written whole
        // (`to_wire(R, desc(value)) + to_wire_full(R, value)`,
        // `serverget.cpp:105-109`), never through `pvMask`, so the op carries
        // no selection mask. Running `request_to_mask` here rejected every
        // RPC whose pvRequest named a field (`RPCBuilder::pvRequest("field(
        // value)")`, or a gateway forwarding a downstream pvRequest) with
        // `EmptyMask` — the RPC prototype is `FieldDesc::Variant`, which
        // matches no named selector — where pvxs answers `Status{}` and runs
        // the call.
        let mask = if kind == OpKind::Rpc {
            crate::proto::BitSet::new()
        } else {
            match crate::pv_request::request_to_mask(&intro, req_desc.as_ref()) {
                Ok(m) => m,
                Err(e) => {
                    // The only variant today is `EmptyMask`: pvRequest
                    // selected no field that exists in the value
                    // descriptor (e.g. `field(noSuch)`). pvxs raises it
                    // as `throw std::runtime_error("pvRequest must select
                    // at least one field")` (`pvrequest.cpp:61-62`), from
                    // inside `request2mask()` — which runs in
                    // `Server{GPR,Monitor}Setup::connect()`
                    // (`serverget.cpp:200`, `servermon.cpp:402`), i.e.
                    // inside the *source's* connect callback, never in the
                    // protocol handler. Who catches it is therefore the
                    // source's choice, and pvxs's own hosting API catches
                    // it on both legs:
                    //   GET/PUT — `serverget.cpp:406-412` wraps
                    //     `chan->onOp(...)` in try/catch and signals a
                    //     remote *op* error.
                    //   MONITOR — `servermon.cpp:591-592` calls
                    //     `chan->onSubscribe(...)` UNGUARDED, but the only
                    //     library source, `SharedPV::Impl::connectSub`
                    //     (`sharedpv.cpp:76,94-101`), catches around
                    //     `conn->connect()` and calls `conn->error(msg)`
                    //     ("not re-throwing for consistency") — an op-level
                    //     Status reply with the circuit left up. pvxs's own
                    //     regression for this throw (`test/testget.cpp:
                    //     380-393`, SharedPV mailbox, `.field("invalid")`)
                    //     asserts exactly that remote error.
                    // So an op-level Status is the parity behaviour and this
                    // is deliberately NOT a fatal `PvaError`. The circuit
                    // reset one can observe against a C QSRV IOC comes from
                    // QSRV's sources alone (`ioc/singlesource.cpp:147`,
                    // `ioc/groupsource.cpp:399` call `connect()` bare, so the
                    // throw unwinds through `servermon.cpp:592` into
                    // `conn.cpp:277-282`'s `bev.reset()`): one client's typo'd
                    // pvRequest drops the shared TCP circuit carrying every
                    // other channel on it. That is an upstream C++ defect, not
                    // a contract to port.
                    //
                    // Pre-fix Rust silently fell back to all-fields, leaking
                    // fields the client didn't request.
                    send_chan_op_error(
                        &chan_tx,
                        kind,
                        ioid,
                        subcmd,
                        Status::error(format!("invalid pvRequest mask: {e}")),
                        order,
                    )
                    .await?;
                    return Ok(());
                }
            }
        };

        // Pipeline flow control is opt-in via pvRequest:
        // `record[pipeline=true,queueSize=N]`. pvxs only enables the
        // credit/ACK window when the client explicitly sets it;
        // applying it unconditionally produced a 5-event-then-stall
        // bug for default `pvmonitor` callers (initial snapshot + 4
        // window credits). Without pipeline=true we don't gate the
        // emit loop — mpsc backpressure remains the only limiter.
        //
        // pvxs reads `record._options.{pipeline,queueSize,ackAny}` only inside
        // `handle_MONITOR` (`servermon.cpp:523-582`); GET/PUT/PUT_GET/RPC never
        // look at them. Parse them for MONITOR only, so every outcome of the
        // reader — including the `NoConvert` throw below — is scoped to the one
        // command pvxs parses them for.
        //
        // That throw is pvxs's third outcome, and the only one that is not a
        // reply: `:556` runs `ackAny.as<std::string>()`, which no `copyOut` arm
        // satisfies for an array / struct / unselected-union `ackAny`. Nothing
        // catches between there and `conn.cpp:277-282`, which logs and calls
        // `bev.reset()` — the circuit is dropped, with no INIT reply and no
        // monitor. A fatal `PvaError` out of this read loop IS that reset. The
        // port used to log a Crit CMD_MESSAGE and serve the monitor on with
        // `ackAt = 1` (R9-33).
        let pipeline_req = match req_value.as_ref().filter(|_| kind == OpKind::Monitor) {
            Some(v) => Some(
                monitor_pipeline_options(v, config.monitor_queue_limit()).map_err(|e| {
                    PvaError::Decode(format!(
                        "MONITOR INIT: record._options.ackAny is not convertible: {e}"
                    ))
                })?,
            ),
            None => None,
        };
        // The ONE negotiated per-op queue limit (pvxs `op->limit`): the
        // server's per-op default unless a valid `record._options.queueSize`
        // replaced it, whether or not pipeline flow control is enabled
        // (`op->limit = qSize` sits outside `if(op->pipeline)`,
        // servermon.cpp:533-543). Captured before `pipeline_req` is consumed
        // by the move below — and read from the Options REGARDLESS of
        // `enabled`, because the limit is not a pipeline property.
        let negotiated_limit = match &pipeline_req {
            Some(MonitorPipelineRequest::Options(o)) => o.queue_size,
            _ => config.monitor_queue_limit(),
        };
        // pvxs `servermon.cpp:537-540`: a MONITOR pipeline request whose
        // PRESENT `queueSize` is invalid (`<2` or unconvertible) is a
        // negotiation error — reject the INIT (`ctrl->error(...)` +
        // `return`) instead of silently downgrading to a non-pipeline
        // monitor. GET/PUT/RPC never negotiate pipeline (pvxs
        // `serverget` ignores these options), so the reject is
        // monitor-only. The text is pvxs's, carrying the offending value;
        // the port used to append an invented "(must be >= 2)" and to reach
        // this path for values pvxs's conversion accepts (a real, a hex or
        // octal string).
        if kind == OpKind::Monitor
            && let Some(MonitorPipelineRequest::Reject(msg)) = &pipeline_req
        {
            send_chan_op_error(
                &chan_tx,
                kind,
                ioid,
                subcmd,
                Status::error(msg.to_string()),
                order,
            )
            .await?;
            return Ok(());
        }
        // pvxs `servermon.cpp:529/542/567/572` — emit `ServerConn::logRemote()`
        // diagnostics for PRESENT-but-invalid monitor options as IOID-tagged
        // CMD_MESSAGE frames before continuing the INIT. MONITOR-only (GET/PUT/
        // RPC never negotiate these options), and after the Reject early-return
        // so a rejected pipeline INIT carries only its op-error (matching pvxs
        // `ctrl->error()`, which emits no logRemote). Borrowed before the move
        // below consumes `pipeline_req`.
        if kind == OpKind::Monitor
            && let Some(MonitorPipelineRequest::Options(o)) = &pipeline_req
        {
            for diag in &o.diagnostics {
                let frame = build_message_frame(ioid, diag.level, &diag.message, order);
                let _ = chan_tx.send(frame).await;
            }
        }
        let pipeline_opt = match pipeline_req {
            Some(MonitorPipelineRequest::Options(o)) => Some(o),
            _ => None,
        }
        .filter(|o| o.enabled);
        // pvxs `servermon.cpp:483-552` — when the client sets the
        // pipeline bit on MONITOR INIT (`subcmd & 0x80`) it appends a u32
        // `nack` (initial window credit) after the pvRequest. Read and
        // consume those bytes so any data following INIT in the same
        // segment decodes from the correct offset. pvxs initialises
        // `nack = 0` and reads it from the wire only when the bit is set
        // (`:494-496`), then assigns `op->window = nack` unconditionally
        // (`:519`); the negotiated `queueSize` feeds `op->limit`/queue
        // depth only, never the initial window. So an absent initial-nack
        // rider must leave the credit window at 0 — a pipelined monitor
        // then sends nothing until the client grants credit, exactly as
        // pvxs does (which also logs "pipeline monitor w/o initial nack
        // incompatible", `:546-552`). A truncated nack with the bit set is
        // FATAL: pvxs reads it unconditionally and resets the connection
        // on `!M.good()`, so propagating the decode error here tears down
        // the connection (matches `bev.reset()`). It is a framing
        // violation, not a legacy omission.
        let pipeline_initial_nack = parse_monitor_init_nack(kind, subcmd, &mut cur, inbound_order)?;
        let (monitor_window, monitor_window_notify) = if kind == OpKind::Monitor
            && let Some(opt) = pipeline_opt.as_ref()
        {
            // Match pvxs `nack = 0` default + `op->window = nack`: an
            // absent 0x80 initial-nack rider seeds the window to 0 (stall
            // until first MONITOR_ACK), never to `queueSize`.
            let initial = pipeline_initial_nack.unwrap_or(0);
            if initial == 0 {
                warn!(
                    ioid,
                    queue_size = opt.queue_size,
                    "pipeline monitor w/o initial nack incompatible (window starts at 0 until first MONITOR_ACK)"
                );
            }
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
                    // A `record._options._filter` that is syntactically
                    // present but unparseable rejects the MONITOR INIT,
                    // mirroring EPICS `dbChannelCreate()`
                    // (`dbChannel.c:512-529`) and pvxs filter setup.
                    // Fail-open to an unfiltered stream would silently
                    // drop the requested throttling/slicing semantics.
                    match epics_base_rs::server::database::filters::try_parse_filter_chain(&j) {
                        Ok(chain) => Arc::new(chain),
                        Err(e) => {
                            send_chan_op_error(
                                &chan_tx,
                                kind,
                                ioid,
                                subcmd,
                                Status::error(format!("invalid channel filter: {e}")),
                                order,
                            )
                            .await?;
                            return Ok(());
                        }
                    }
                }
                None => Arc::new(epics_base_rs::server::database::filters::FilterChain::new()),
            }
        } else {
            Arc::new(epics_base_rs::server::database::filters::FilterChain::new())
        };

        // Stash the INIT pvRequest so the data-phase
        // dispatch can forward it through `ChannelContext.pv_request`.
        // PUT needs `record._options.process|block`; MONITOR needs
        // `record._options.DBE` (and other per-op stream tuning that
        // wasn't already consumed for mask/pipeline/filter parsing).
        // RPC needs the create-time pvRequest preserved separately from
        // the EXEC argument (pvxs serverget.cpp:388-391 stores the INIT
        // pvRequest and hands it to the operation controller) so a source
        // — and a gateway forwarding `createChannelRPC(..., pvRequest)` —
        // can inspect it. GET doesn't read per-op options from this value
        // beyond what was already extracted, so we don't pay the clone.
        let stashed_pv_request = match kind {
            OpKind::Put | OpKind::Monitor | OpKind::Rpc => req_value.clone(),
            _ => None,
        };

        // capture the event-affecting MONITOR pvRequest
        // options so the START path can hand them to the source's
        // `subscribe_*_checked_opts`. `queue_size` carries the
        // client-requested `queueSize` whether or not pipeline mode is
        // enabled (pvxs `op->limit = qSize` is honoured for plain
        // monitors too, servermon.cpp:533-543) — the START path reads it
        // as the per-op squash threshold. `server_filter` reflects
        // whether a non-empty `_filter` chain was present.
        let monitor_options = if kind == OpKind::Monitor {
            crate::server_native::source::MonitorOptions {
                pipeline: pipeline_opt.is_some(),
                queue_size: negotiated_limit,
                server_filter: !monitor_filters.is_empty(),
            }
        } else {
            crate::server_native::source::MonitorOptions::default()
        };

        // Capture the source's pipeline-window
        // watermark levels at INIT so the subscriber loop (LOW) and the
        // ACK dispatch (HIGH) evaluate the same `(low, high)` against the
        // shared hysteresis flag. pvxs `servermon.cpp:332-333`: the
        // pipeline `ackAny`/`ackAt` threshold caps those levels at
        // `ackAt - 1`. Clamping here, once, is what makes both crossings
        // honor `ackAny` identically (the subscriber loop reads the
        // value threaded out of this `OpState`, not a fresh source read).
        let monitor_wm = if kind == OpKind::Monitor {
            clamp_watermarks(
                source.monitor_watermarks(&ch.name).await,
                pipeline_opt.as_ref().map(|p| p.ack_at),
            )
        } else {
            None
        };

        // pvxs runs the SOURCE's `record._options` read at the top of
        // `onSubscribe` — before `connect()` emits the INIT reply — through the
        // THROWING `Value::as<T>()` (`ioc/singlesource.cpp:117-140`). An option
        // whose storage no `copyOut` arm converts raises `NoConvert`, nothing
        // catches it on the way out of `handle_MONITOR`, and
        // `conn.cpp:277-282` calls `bev.reset()`. The port opens the
        // subscription at START, not INIT, so the failing half of `onSubscribe`
        // is its own INIT-time hook: `check_monitor_request`. (R9-35.)
        //
        // DEVIATION from C++, deliberate — CBUG-C2. QSRV's `bev.reset()` is a
        // per-operation failure escalated to a transport reset: one client's
        // malformed `record._options` drops the shared TCP circuit, and with it
        // every OTHER channel multiplexed on it. Through a gateway that is one
        // user's field typo disconnecting every other user. So the hook's `Err`
        // is an `OpError`, answered here the same way as the pvRequest-mask
        // failure above: an INIT reply carrying an error `Status`, the op left
        // unregistered, the circuit and its other channels untouched. This is
        // also what pvxs's OWN library source does for the same throw
        // (`sharedpv.cpp:94-101` catches around `connect()` and calls
        // `conn->error(msg)`); only QSRV's sources leave it bare.
        if kind == OpKind::Monitor {
            let init_ctx = crate::server_native::source::ChannelContext {
                peer,
                account: cred.account.clone(),
                method: cred.method.clone(),
                host: cred.host.clone(),
                authority: cred.authority.clone(),
                roles: cred.roles.clone(),
                pv_request: req_value.clone(),
                log: Default::default(),
            };
            let checked = source
                .access_gate()
                .check_with_roles(
                    &ch.name,
                    &init_ctx.host,
                    &init_ctx.account,
                    &init_ctx.roles,
                    &init_ctx.method,
                    &init_ctx.authority,
                )
                .await;
            if let Err(e) = source.check_monitor_request(&checked, &init_ctx).await {
                send_chan_op_error(&chan_tx, kind, ioid, subcmd, e.wire_status(), order).await?;
                return Ok(());
            }
            // R10-37: the source's `onSubscribe` diagnostics are INIT-time.
            // pvxs records them INSIDE `onSubscribe` — `singlesource.cpp:129`'s
            // `record._options.DBE=… selects empty mask` is written before
            // `connect()` emits the INIT reply — so the client sees the
            // CMD_MESSAGE ahead of that reply. The port opens the subscription
            // at START, and used to let this hook parse against a discarded log
            // and re-parse at START to log there, putting the message AFTER the
            // INIT reply (and emitting it for group / native-PVA channels, whose
            // pvxs sources never read DBE at all). Draining here, before the
            // reply is built, is pvxs's order. Only on the Ok path: the failing
            // DBE returns above with an error INIT reply, and pvxs's throw
            // likewise happens before its logRemote, so it logs nothing either.
            flush_remote_log(&init_ctx.log, ioid, order, &chan_tx).await;
        }

        ch.ops.insert(
            ioid,
            OpState {
                intro: intro.clone(),
                kind,
                monitor_started: false,
                monitor_abort: None,
                mask,
                put_mask: None,
                monitor_window,
                monitor_window_notify,
                monitor_paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                monitor_resume: Arc::new(tokio::sync::Notify::new()),
                monitor_wm,
                monitor_wm_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
                monitor_op_id: next_op_id(),
                monitor_filters,
                pv_request: stashed_pv_request,
                monitor_options,
                data_task_abort: None,
                monitor_start_ctl: None,
                exec_state: ExecState::Idle,
                last_request: false,
            },
        );

        // Subscribe + spawn the MONITOR subscriber at INIT (pvxs
        // `onSubscribe` registers the upstream at INIT, servermon.cpp:591), so
        // posts arriving in the INIT->START window are accrued into the op's
        // bounded FIFO rather than lost. The task stays Idle (emits nothing)
        // until the first MONITOR START flips its `monitor_exec` watch. Install
        // the abort guard + start-control in the SAME synchronous step as the
        // spawn (teardown invariant: task-spawned <=> abort-installed), so a
        // DESTROY-before-START always tears the upstream down.
        if kind == OpKind::Monitor {
            let pv_name = ch.name.clone();
            // Per-op squash threshold: the negotiated limit resolved at INIT
            // (`monitor_options.queue_size` — the server's per-op default
            // unless a valid `record._options.queueSize` replaced it). pvxs
            // squashes against that one `op->limit` for pipeline and plain
            // monitors alike (`queue.size() < limit`, servermon.cpp:273).
            let queue_depth = ch
                .ops
                .get(&ioid)
                .map(|s| s.monitor_options.queue_size as usize)
                .unwrap_or(config.monitor_queue_depth);
            let high_watermark = config.monitor_high_watermark;
            // Read the per-op state back out of the just-inserted `OpState`
            // (its construction consumed the `mask`/window/filter locals) into
            // a self-contained args bundle, so the spawn holds no `ch` borrow.
            let args = ch.ops.get(&ioid).map(|s| {
                // ACF-aware MONITOR: forward the INIT pvRequest so the source
                // can honor `record._options.DBE` (per-op event-mask selection,
                // pvxs singlesource.cpp:115); data-phase START/ACK frames are
                // pure stream control and carry no per-op options.
                let mon_ctx = crate::server_native::source::ChannelContext {
                    peer,
                    account: cred.account.clone(),
                    method: cred.method.clone(),
                    host: cred.host.clone(),
                    authority: cred.authority.clone(),
                    roles: cred.roles.clone(),
                    pv_request: s.pv_request.clone(),
                    log: Default::default(),
                };
                MonitorSubscriberArgs {
                    sid,
                    ioid,
                    pv_name: pv_name.clone(),
                    intro: intro.clone(),
                    mask: s.mask.clone(),
                    tx: chan_tx.clone(),
                    src: source.clone(),
                    queue_depth,
                    high_watermark,
                    mon_ctx,
                    window: s.monitor_window.clone(),
                    window_notify: s.monitor_window_notify.clone(),
                    filters: s.monitor_filters.clone(),
                    monitor_options: s.monitor_options.clone(),
                    wm_seq: s.monitor_wm_seq.clone(),
                    monitor_op_id: s.monitor_op_id,
                    wm_levels: s.monitor_wm,
                    mon_fin_tx: mon_fin_tx.clone(),
                    out_order: out_order.clone(),
                }
            });
            if let Some(args) = args {
                let (join, start_ctl) = spawn_monitor_subscriber(args);
                if let Some(s) = ch.ops.get_mut(&ioid) {
                    s.monitor_started = true;
                    s.monitor_abort = Some(Arc::new(AbortOnDrop(join.abort_handle())));
                    s.monitor_start_ctl = Some(start_ctl);
                }
            }
        }

        // Build INIT response: ioid + subcmd + status + introspection
        let cmd = kind.command();

        // pvxs derives the INIT reply subcommand from operation STATE, not by
        // echoing the request byte. GET/PUT/RPC/PUT_GET replies echo the
        // stored `op->subcmd` (`serverget.cpp:83`), which for an INIT request
        // is exactly `0x08`. The MONITOR reply path differs: `doReply` sets
        // `subcmd = 0x08` unconditionally for the Creating→Idle INIT frame
        // (`servermon.cpp:133-135`) and NEVER sets the `0x80` pipeline bit a
        // client put on the inbound MONITOR INIT (`0x88`). Echoing the request
        // here would ship `0x88`, which a pvAccessCPP client decodes as a
        // pipeline-flagged INIT reply. Mirror pvxs: a monitor INIT replies
        // exactly `0x08`; the other op kinds echo their (already `0x08`)
        // inbound INIT subcmd, matching the data-phase echo `send_op_error`
        // documents (hardcoding `0x08` for those would mis-frame data-phase).
        let reply_subcmd = if matches!(kind, OpKind::Monitor) {
            0x08
        } else {
            subcmd
        };

        let mut payload = Vec::new();
        payload.put_u32(ioid, order);
        payload.put_u8(reply_subcmd);
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
        // Attribute the op INIT reply to the channel (pvxs
        // `chan->statTx += enqueueTxBody(cmd)`, serverget.cpp:124).
        let _ = chan_tx.send(buf).await;
        return Ok(());
    }

    // Data phase
    let op = ch.ops.get(&ioid).cloned();
    let (intro, mask, init_pv_request) = match op {
        Some(o) => {
            // data/control frames must match the operation
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
            // silently drop — pvxs serverget.cpp:423-428 returns without reply here
            // to handle the DESTROY_REQUEST/in-flight-frame race.
            return Ok(());
        }
    };

    match kind {
        OpKind::Get => {
            // spawn the data-phase work so the read loop can
            // continue parsing frames while the source future runs.
            let pv_name = ch.name.clone();
            let src = source.clone();
            let tx_clone = chan_tx.clone();
            let intro_t = intro.clone();
            let mask_t = mask.clone();
            let cred_account = cred.account.clone();
            let cred_method = cred.method.clone();
            let cred_host = cred.host.clone();
            let cred_authority = cred.authority.clone();
            let cred_roles = cred.roles.clone();
            // forward the decoded INIT pvRequest into the GET
            // context so QSRV group GET honors `record._options`
            // (e.g. `atomic`). Previously dropped here as `None`.
            let init_pv_request_t = init_pv_request.clone();
            // pvxs `serverget.cpp:467-476` runs a data-phase EXEC
            // only when the op is `Idle`, flips it to `Executing`, and
            // IGNORES a second EXEC that arrives while the first task is in
            // flight (`:511-514`) — it does NOT abort the in-flight task.
            let op_id = match begin_exec(ch, ioid) {
                Some(id) => id,
                None => {
                    debug!(ioid, "GET EXEC ignored: op already executing");
                    return Ok(());
                }
            };
            let exec_fin = ExecFinished {
                sid,
                ioid,
                op_id,
                success: false,
            };
            let exec_fin_tx_task = exec_fin_tx.clone();
            let abort = poll_inline_or_spawn(async move {
                // returns this op to `Idle` (via the read-loop owner)
                // when the task ends, so a later explicit re-EXEC is accepted.
                let mut exec_fin_guard = ExecFinishGuard {
                    tx: exec_fin_tx_task,
                    fin: exec_fin,
                };
                let ctx = crate::server_native::source::ChannelContext {
                    peer,
                    account: cred_account,
                    method: cred_method,
                    host: cred_host,
                    authority: cred_authority,
                    roles: cred_roles,
                    pv_request: init_pv_request_t,
                    log: Default::default(),
                };
                let checked = src
                    .access_gate()
                    .check_with_roles(
                        &pv_name,
                        &ctx.host,
                        &ctx.account,
                        &ctx.roles,
                        &ctx.method,
                        &ctx.authority,
                    )
                    .await;
                // a panic in the user GET handler becomes a
                // data-phase error reply instead of skipping the reply below.
                let op_log = ctx.log.clone();
                let got = catch_handler_panic(src.read_checked(checked, ctx)).await;
                flush_remote_log(&op_log, ioid, order, &tx_clone).await;
                let read = match got {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        let _ = send_chan_op_error(
                            &tx_clone,
                            OpKind::Get,
                            ioid,
                            subcmd,
                            Status::error("PV not found"),
                            order,
                        )
                        .await;
                        return;
                    }
                    Err(msg) => {
                        let _ = send_chan_op_error(
                            &tx_clone,
                            OpKind::Get,
                            ioid,
                            subcmd,
                            Status::error(msg.to_string()),
                            order,
                        )
                        .await;
                        return;
                    }
                };
                // source-side mismatch gate.
                if let Err(e) = crate::pvdata::value_matches_descriptor(&read.value, &intro_t) {
                    let _ = send_chan_op_error(
                        &tx_clone,
                        OpKind::Get,
                        ioid,
                        subcmd,
                        Status::error(format!(
                            "source value does not match opened descriptor: {e}"
                        )),
                        order,
                    )
                    .await;
                    return;
                }
                let mut payload = Vec::new();
                payload.put_u32(ioid, order);
                payload.put_u8(subcmd);
                Status::ok().write_into(order, &mut payload);
                // pvxs `serverget.cpp:104`: `to_wire_valid(R, value, &pvMask)`
                // — the leaves the source assigned into the reply value, not
                // every leaf the request selected.
                let changed = read_changed_bitset(&intro_t, &mask_t, read.marked.as_deref());
                changed.write_into(order, &mut payload);
                crate::pvdata::encode::encode_pv_field_with_bitset(
                    &read.value,
                    &intro_t,
                    &changed,
                    0,
                    order,
                    &mut payload,
                );
                let h =
                    PvaHeader::application(true, order, Command::Get.code(), payload.len() as u32);
                let mut buf = Vec::new();
                h.write_into(&mut buf);
                buf.extend_from_slice(&payload);
                // Successful GET data reply: a last_request op is cleaned up by
                // the completion owner (pvxs serverget.cpp:112-114). Every
                // error path above returned before reaching here.
                exec_fin_guard.mark_success();
                let _ = tx_clone.send(buf).await;
            });
            finish_exec_data_task(ch, ioid, subcmd, abort);
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
                // spawn GET-for-PUT-readback — blocks on
                // source.get_value_checked which can be slow.
                let pv_name = ch.name.clone();
                let src = source.clone();
                let tx_clone = chan_tx.clone();
                let intro_t = intro.clone();
                let mask_t = mask.clone();
                let cred_account = cred.account.clone();
                let cred_method = cred.method.clone();
                let cred_host = cred.host.clone();
                let cred_authority = cred.authority.clone();
                let cred_roles = cred.roles.clone();
                // forward the INIT pvRequest into the PUT
                // readback GET context so the readback honors the
                // same `record._options` the GET path would.
                let init_pv_request_t = init_pv_request.clone();
                // ignore a second EXEC while the readback task is in
                // flight rather than aborting it (pvxs `serverget.cpp:511-514`).
                let op_id = match begin_exec(ch, ioid) {
                    Some(id) => id,
                    None => {
                        debug!(ioid, "PUT readback EXEC ignored: op already executing");
                        return Ok(());
                    }
                };
                let exec_fin = ExecFinished {
                    sid,
                    ioid,
                    op_id,
                    success: false,
                };
                let exec_fin_tx_task = exec_fin_tx.clone();
                let abort = poll_inline_or_spawn(async move {
                    let mut exec_fin_guard = ExecFinishGuard {
                        tx: exec_fin_tx_task,
                        fin: exec_fin,
                    };
                    let ctx = crate::server_native::source::ChannelContext {
                        peer,
                        account: cred_account,
                        method: cred_method,
                        host: cred_host,
                        authority: cred_authority,
                        roles: cred_roles,
                        pv_request: init_pv_request_t,
                        log: Default::default(),
                    };
                    let checked = src
                        .access_gate()
                        .check_with_roles(
                            &pv_name,
                            &ctx.host,
                            &ctx.account,
                            &ctx.roles,
                            &ctx.method,
                            &ctx.authority,
                        )
                        .await;
                    // a panic in the user GET (PUT readback)
                    // handler becomes a data-phase error reply instead of
                    // skipping the reply below.
                    let op_log = ctx.log.clone();
                    let got = catch_handler_panic(src.read_checked(checked, ctx)).await;
                    flush_remote_log(&op_log, ioid, order, &tx_clone).await;
                    let read = match got {
                        Ok(Some(v)) => v,
                        Ok(None) => {
                            let _ = send_chan_op_error(
                                &tx_clone,
                                OpKind::Put,
                                ioid,
                                subcmd,
                                Status::error("PV not found"),
                                order,
                            )
                            .await;
                            return;
                        }
                        Err(msg) => {
                            let _ = send_chan_op_error(
                                &tx_clone,
                                OpKind::Put,
                                ioid,
                                subcmd,
                                Status::error(msg.to_string()),
                                order,
                            )
                            .await;
                            return;
                        }
                    };
                    let mut payload = Vec::new();
                    payload.put_u32(ioid, order);
                    payload.put_u8(subcmd);
                    Status::ok().write_into(order, &mut payload);
                    let changed = read_changed_bitset(&intro_t, &mask_t, read.marked.as_deref());
                    changed.write_into(order, &mut payload);
                    crate::pvdata::encode::encode_pv_field_with_bitset(
                        &read.value,
                        &intro_t,
                        &changed,
                        0,
                        order,
                        &mut payload,
                    );
                    let h = PvaHeader::application(
                        true,
                        order,
                        Command::Put.code(),
                        payload.len() as u32,
                    );
                    let mut buf = Vec::new();
                    h.write_into(&mut buf);
                    buf.extend_from_slice(&payload);
                    // Successful PUT/Get readback reply (subcmd & 0x40): a
                    // last_request op is cleaned up by the completion owner;
                    // every error path above returned before reaching here.
                    exec_fin_guard.mark_success();
                    let _ = tx_clone.send(buf).await;
                });
                finish_exec_data_task(ch, ioid, subcmd, abort);
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
            let changed = BitSet::decode(&mut cur, inbound_order)
                .map_err(|e| PvaError::Decode(e.to_string()))?;
            // pvxs `serverget.cpp:488-492` calls `onPut` immediately
            // on every CMD_PUT !init — the client's autoExec setting
            // is purely a client-side timing knob (clientget.cpp:213)
            // for whether the PUT EXEC fires automatically after INIT
            // or waits for `reExec()`. Each EXEC frame still carries
            // exactly one value and triggers exactly one write.
            let delta = decode_pv_field_with_bitset_cached(
                &intro,
                &changed,
                0,
                &mut cur,
                inbound_order,
                decode_cache,
            )
            .map_err(|e| PvaError::Decode(format!("PUT requires a value payload: {e}")))?;
            let pv_name = ch.name.clone();
            // spawn PUT exec — put_delta_checked can be slow.
            // Decode frame data synchronously (above) so the cursor is
            // consumed before returning; source calls happen in the task.
            let src = source.clone();
            let tx_clone = chan_tx.clone();
            let intro_t = intro.clone();
            let cred_account = cred.account.clone();
            let cred_method = cred.method.clone();
            let cred_host = cred.host.clone();
            let cred_authority = cred.authority.clone();
            let cred_roles = cred.roles.clone();
            let init_pv_request_t = init_pv_request.clone();
            // ignore a second PUT EXEC while the first write is in
            // flight rather than aborting it (pvxs `serverget.cpp:511-514`).
            let op_id = match begin_exec(ch, ioid) {
                Some(id) => id,
                None => {
                    debug!(ioid, "PUT EXEC ignored: op already executing");
                    return Ok(());
                }
            };
            let exec_fin = ExecFinished {
                sid,
                ioid,
                op_id,
                success: false,
            };
            let exec_fin_tx_task = exec_fin_tx.clone();
            let abort = poll_inline_or_spawn(async move {
                let mut exec_fin_guard = ExecFinishGuard {
                    tx: exec_fin_tx_task,
                    fin: exec_fin,
                };
                let ctx = crate::server_native::source::ChannelContext {
                    peer,
                    account: cred_account,
                    method: cred_method,
                    host: cred_host,
                    authority: cred_authority,
                    roles: cred_roles,
                    pv_request: init_pv_request_t,
                    log: Default::default(),
                };
                // The source's `RemoteLogger` sink for this PUT, drained onto
                // the wire below before the reply (pvxs
                // `groupsource.cpp:560` / `iocsource.cpp:447` warn from
                // inside the PUT and reply afterwards).
                let op_log = ctx.log.clone();
                let result = {
                    let checked = src
                        .access_gate()
                        .check_with_roles(
                            &pv_name,
                            &ctx.host,
                            &ctx.account,
                            &ctx.roles,
                            &ctx.method,
                            &ctx.authority,
                        )
                        .await;
                    // a panic in the user PUT handler becomes an
                    // error reply instead of skipping the reply below.
                    catch_handler_panic(src.put_delta_checked(
                        checked,
                        intro_t.clone(),
                        changed.clone(),
                        delta,
                        ctx.clone(),
                    ))
                    .await
                    .map_err(|e| OpError::failed(e))
                    .and_then(|r| r)
                };
                flush_remote_log(&op_log, ioid, order, &tx_clone).await;
                let mut payload = Vec::new();
                payload.put_u32(ioid, order);
                payload.put_u8(subcmd);
                // `replied_ok` mirrors the single status word written below:
                // pvxs `ServerGPR::doReply` decides the op disposition from
                // `sts.isSuccess()` (serverget.cpp:86-116). A successful
                // last_request PUT/PUT_GET is cleaned up by the completion
                // owner; an error reply keeps the op Idle with the sticky
                // marker for a later EXEC.
                let replied_ok = match result {
                    Ok(()) => {
                        if subcmd & 0x40 != 0 {
                            // PUT_GET readback: build readback
                            // before emitting status so we know whether
                            // READ was denied and can emit an empty
                            // bitset instead of truncating the wire.
                            let read_checked = src
                                .access_gate()
                                .check_with_roles(
                                    &pv_name,
                                    &ctx.host,
                                    &ctx.account,
                                    &ctx.roles,
                                    &ctx.method,
                                    &ctx.authority,
                                )
                                .await;
                            // a panic in the user GET (PUT_GET
                            // readback) handler becomes an error reply instead
                            // of skipping the reply below.
                            let readback =
                                catch_handler_panic(src.get_value_checked(read_checked, ctx)).await;
                            flush_remote_log(&op_log, ioid, order, &tx_clone).await;
                            match readback {
                                Ok(Some(v)) => {
                                    Status::ok().write_into(order, &mut payload);
                                    let bits = BitSet::all_set(intro_t.total_bits());
                                    bits.write_into(order, &mut payload);
                                    encode_pv_field(&v, &intro_t, order, &mut payload);
                                    true
                                }
                                Ok(None) => {
                                    Status::ok().write_into(order, &mut payload);
                                    let empty = BitSet::with_capacity(intro_t.total_bits());
                                    empty.write_into(order, &mut payload);
                                    true
                                }
                                Err(msg) => {
                                    Status::error(msg).write_into(order, &mut payload);
                                    false
                                }
                            }
                        } else {
                            Status::ok().write_into(order, &mut payload);
                            true
                        }
                    }
                    Err(e) => {
                        e.wire_status().write_into(order, &mut payload);
                        false
                    }
                };
                let h =
                    PvaHeader::application(true, order, Command::Put.code(), payload.len() as u32);
                let mut buf = Vec::new();
                h.write_into(&mut buf);
                buf.extend_from_slice(&payload);
                if replied_ok {
                    exec_fin_guard.mark_success();
                }
                let _ = tx_clone.send(buf).await;
            });
            finish_exec_data_task(ch, ioid, subcmd, abort);
        }
        OpKind::Monitor => {
            // pvxs `servermon.cpp:643-708` splits the data-phase MONITOR
            // subcmd into three INDEPENDENT actions, each gated by its
            // own bit — they are not interchangeable triggers:
            //   * ACK  (`0x80`): refill the pipeline window only. Never
            //     moves the op out of Idle and never fires onStart.
            //   * START/STOP (`0x04`): set Executing/Idle and fire
            //     `onStart(start)`, where `start = subcmd & 0x40`
            //     (`servermon.cpp:671-683`). So START is `0x44` and STOP
            //     is `0x04`; the `0x40` bit alone, without `0x04`, is
            //     NOT a start.
            //   * DESTROY (`0x10`): tear the op down, equivalent to the
            //     dedicated DESTROY_REQUEST command. pvxs accepts destroy
            //     in any non-INIT MONITOR message (`servermon.cpp:640-642`,
            //     :691-708) — handled below, after ack/start so a combined
            //     frame starts-then-destroys in pvxs order.
            // A frame carrying none of these bits (notably plain `0x00`)
            // performs no stream-control action — pvxs leaves the monitor
            // Idle. Gating the task spawn and the onStart edge on a real
            // START (`is_start`) rather than the old
            // "0x40 | ack | 0x00" union keeps the monitor idle until the
            // client actually starts delivery.
            let is_ack = subcmd & 0x80 != 0;
            let is_start_stop = subcmd & 0x04 != 0;
            let is_start = is_start_stop && (subcmd & 0x40 != 0);
            let is_stop = is_start_stop && (subcmd & 0x40 == 0);
            let is_destroy = subcmd & 0x10 != 0;
            // Validate-before-side-effect: decode and validate the optional
            // ACK count BEFORE any START/STOP or source callback.
            //
            // pvxs `servermon.cpp:599-608` reads `from_wire(M, nack)` when
            // `subcmd & 0x80` and, on a truncated frame (`!M.good()`), calls
            // `bev.reset()` and returns BEFORE op lookup, ACK refill,
            // `onHighMark`, `onStart`, or any state change. A malformed
            // combined frame — `0xC4` (ACK|START) or `0x84` (ACK|STOP) with no
            // ACK `u32` — must therefore NOT fire `notify_monitor_start` or
            // mutate `monitor_paused`/the resume waiters before this decode.
            // Hoisting the decode above the START/STOP block makes those side
            // effects unreachable on a bad ACK. `cur` is consumed by nothing
            // else in this arm, so this is the sole frame-payload read; a
            // missing/truncated count propagates a Decode error that resets the
            // connection. Never fabricate credits — the old `unwrap_or(4)`
            // corrupted the flow-control window.
            let ack_count: Option<u32> = if is_ack {
                Some(cur.get_u32(inbound_order).map_err(|e| {
                    PvaError::Decode(format!(
                        "malformed MONITOR ACK (ioid {ioid}): missing u32 ack-count: {e}"
                    ))
                })?)
            } else {
                None
            };

            // ACK refill — applied BEFORE START/STOP to match pvxs order
            // (servermon.cpp:643-689 applies ACK refill then START/STOP, so
            // `onHighMark` precedes `onStart` for a well-formed combined
            // frame). The ACK count was validated above; here we add it to the
            // pipeline window and pulse the notify so a paused subscriber wakes
            // and resumes emission. ACKs can arrive before OR after the START —
            // we always honour them.
            if let Some(ack_count) = ack_count {
                // fire HIGH (resume) from the ACK path —
                // pvxs `servermon.cpp:653-666` fires `onHighMark` when
                // ACKs add enough credit. A gateway source that paused
                // its single upstream monitor on LOW receives no further
                // events while paused, so the event-loop HIGH check could
                // never re-fire; the resume MUST be driven by the credit
                // refill here. `fire_high` (the crossing's ordering
                // token) is computed under the `op` borrow, then the
                // callback runs after it is dropped so `source` can
                // borrow `ch.name` freely.
                let mut fire_high: Option<(u64, u64)> = None;
                if let Some(op) = ch.ops.get(&ioid) {
                    if let (Some(w), Some(n)) = (
                        op.monitor_window.as_ref(),
                        op.monitor_window_notify.as_ref(),
                    ) {
                        // pvxs `servermon.cpp:653` refills a `size_t`
                        // window (`op->window += nack`) and tests that SAME
                        // post-add value against `op->high`. Mirror it with
                        // a SATURATING add: a raw `AtomicU32::fetch_add`
                        // wraps past `u32::MAX`, leaving the stored credit
                        // (which `acquire` decrements) far below the value
                        // the HIGH-watermark check computed in `usize` — the
                        // two then disagree on how much credit exists. An
                        // un-acked window above `u32::MAX` is nonsensical
                        // (it counts in-flight monitor updates), so capping
                        // is strictly safer than wrapping and keeps the
                        // stored window and the watermark decision derived
                        // from one number.
                        let mut prev = w.load(std::sync::atomic::Ordering::Relaxed);
                        let w_now = loop {
                            let next = prev.saturating_add(ack_count);
                            match w.compare_exchange_weak(
                                prev,
                                next,
                                std::sync::atomic::Ordering::Relaxed,
                                std::sync::atomic::Ordering::Relaxed,
                            ) {
                                Ok(_) => break next,
                                Err(observed) => prev = observed,
                            }
                        };
                        if prev == 0 {
                            n.notify_waiters();
                        }
                        // HIGH fires once per crossing: the refilled
                        // window stands above `high`. `cross_watermark`
                        // both checks-and-marks the crossing and mints the
                        // ordering token in one CAS, returning `Some(seq)`
                        // exactly on the below→above edge. The companion
                        // LOW (event loop) crosses back when a DATA
                        // emission drains to `<= low`. The check uses the
                        // saturated `w_now` — the same value now stored —
                        // so credit and watermark cannot diverge.
                        if let Some((_lo, hi)) = op.monitor_wm {
                            if w_now as usize > hi {
                                fire_high = cross_watermark(&op.monitor_wm_seq, true)
                                    .map(|seq| (seq, op.monitor_op_id));
                            }
                        }
                    }
                }
                if let Some((seq, op_id)) = fire_high {
                    // thread this connection's
                    // credential context so a gateway scopes the resume to
                    // the firing credential's own upstream cache layer, the
                    // crossing `seq` so it orders this op's transitions, and
                    // `op_id` so it releases THIS op's pause vote (not a
                    // co-subscriber's). `pv_request` is irrelevant to cache
                    // selection, so it is omitted.
                    let wm_ctx = crate::server_native::source::ChannelContext {
                        peer,
                        account: cred.account.clone(),
                        method: cred.method.clone(),
                        host: cred.host.clone(),
                        authority: cred.authority.clone(),
                        roles: cred.roles.clone(),
                        pv_request: None,
                        log: Default::default(),
                    };
                    source.notify_watermark(
                        &ch.name,
                        &wm_ctx,
                        crate::server_native::source::WatermarkEvent {
                            op_id,
                            seq,
                            kind: crate::server_native::source::WatermarkKind::Resume,
                        },
                    );
                }
            }

            // START / STOP — applied AFTER ACK refill (pvxs
            // servermon.cpp:671-689 sets Executing/Idle and fires `onStart`
            // after the ACK block). The subscriber task is spawned at
            // INIT (the upstream is subscribed at INIT so INIT->START posts are
            // accrued into the op's bounded FIFO), so START/STOP here only flip
            // the Executing edge on the op's single start-control owner; the
            // subscriber's emit gate (its `monitor_exec` watch) follows that
            // edge. `monitor_paused` mirrors the Idle/paused state for the
            // cancel-parity tests.
            if let Some(op) = ch.ops.get(&ioid) {
                if is_stop {
                    op.monitor_paused
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    // Executing->Idle: the consumer suspends emission, holding
                    // the FIFO (up to queueSize) for the next START. The op's
                    // single start-control owner fires notify_monitor_start(false)
                    // on the edge so a gateway can suspend its upstream.
                    if let Some(ctl) = &op.monitor_start_ctl {
                        ctl.set(false);
                    }
                } else if is_start {
                    op.monitor_paused
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    // Idle->Executing on EVERY start. `MonitorStartControl::set`
                    // is edge-triggered (only fires the source edge + watch
                    // publish on a real transition), so a redundant START on an
                    // already-executing monitor is a no-op. The FIRST start (task
                    // already spawned at INIT) is what flips the watch to
                    // Executing and releases the accrued INIT->START backlog.
                    if let Some(ctl) = &op.monitor_start_ctl {
                        ctl.set(true);
                    }
                }
            }

            // pvxs `servermon.cpp:691-708`: the destroy bit (`0x10`) on any
            // non-INIT MONITOR frame frees the op exactly like the dedicated
            // DESTROY_REQUEST command — pvAccessCPP "will accept destroy in
            // any !INIT message". Removing the op from `ch.ops` drops its
            // `monitor_abort` (aborting the subscriber task once the last
            // clone falls) and `monitor_start_ctl` (firing the gateway's
            // pause vote withdraw); no reply is emitted. Checked after
            // ack/start so a combined frame starts-then-destroys, and the
            // IOID is now free for a fresh INIT (the duplicate-live-op guard
            // no longer trips).
            if is_destroy {
                ch.ops.remove(&ioid);
            }
        }
        OpKind::Rpc => {
            // RPC DATA request from client: `type(arg) + full_value(arg)`.
            // pvxs clientget.cpp:307-311 — `to_wire(R, type); to_wire_full(R, arg)`.
            // Decode the argument inline (before spawning) because the cursor
            // is borrowed from the frame which lives on the read-loop stack.
            // An absent body or a NULL (0xFF) type code is a parameterless
            // RPC; a present-but-malformed descriptor or value is a
            // connection-fatal decode error (pvxs `serverget.cpp:454-458`
            // `bev.reset()`), not a fabricated `Null` argument.
            let (req_desc, req_value) = decode_rpc_exec_arg(&mut cur, inbound_order, decode_cache)
                .map_err(PvaError::Decode)?;
            let pv_name = ch.name.clone();
            let _ = intro;
            let src = source.clone();
            let tx_clone = chan_tx.clone();
            let rpc_ctx_val = crate::server_native::source::ChannelContext {
                peer,
                account: cred.account.clone(),
                method: cred.method.clone(),
                host: cred.host.clone(),
                authority: cred.authority.clone(),
                roles: cred.roles.clone(),
                // RPC INIT pvRequest, preserved from the op state — distinct
                // from the `(req_desc, req_value)` EXEC argument decoded
                // above. A source (or gateway) can now inspect the
                // create-time request (pvxs serverget.cpp:388-391).
                pv_request: init_pv_request.clone(),
                log: Default::default(),
            };
            // ignore a second RPC EXEC while the first call is in
            // flight rather than aborting it (pvxs `serverget.cpp:511-514`).
            let op_id = match begin_exec(ch, ioid) {
                Some(id) => id,
                None => {
                    debug!(ioid, "RPC EXEC ignored: op already executing");
                    return Ok(());
                }
            };
            let exec_fin = ExecFinished {
                sid,
                ioid,
                op_id,
                success: false,
            };
            let exec_fin_tx_task = exec_fin_tx.clone();
            let abort = poll_inline_or_spawn(async move {
                let mut exec_fin_guard = ExecFinishGuard {
                    tx: exec_fin_tx_task,
                    fin: exec_fin,
                };
                let rpc_checked = src
                    .access_gate()
                    .check_with_roles(
                        &pv_name,
                        &rpc_ctx_val.host,
                        &rpc_ctx_val.account,
                        &rpc_ctx_val.roles,
                        &rpc_ctx_val.method,
                        &rpc_ctx_val.authority,
                    )
                    .await;
                // a panic in the user RPC handler becomes an error
                // reply instead of skipping the reply below.
                let op_log = rpc_ctx_val.log.clone();
                let result = catch_handler_panic(src.rpc_checked(
                    rpc_checked,
                    req_desc,
                    req_value,
                    rpc_ctx_val,
                ))
                .await
                .map_err(|e| OpError::failed(e))
                .and_then(|r| r);
                flush_remote_log(&op_log, ioid, order, &tx_clone).await;

                let mut payload = Vec::new();
                payload.put_u32(ioid, order);
                // pvxs `serverget.cpp:83` echoes the request subcmd.
                payload.put_u8(subcmd);
                // `replied_ok` mirrors the status word written below; pvxs
                // `ServerGPR::doReply` cleans up a last_request RPC only after
                // success and otherwise returns it to Idle (serverget.cpp:86-116).
                let replied_ok = match result {
                    // pvxs `serverget.cpp:105-109`:
                    //     auto type = Value::Helper::desc(value);
                    //     to_wire(R, type);
                    //     if(value) to_wire_full(R, value);
                    // — the descriptor is ALWAYS written, the value body only
                    // when the reply carries one. `ExecOp::reply()` (the
                    // no-argument overload, `srvcommon.h:108`) replies with a
                    // default-constructed `Value`, so `desc()` is `nullptr`
                    // and `to_wire(Buf&, const FieldDesc*)` writes the single
                    // `0xFF` NULL type code (`dataencode.cpp:29-33`).
                    Ok(RpcReply::Empty) => {
                        Status::ok().write_into(order, &mut payload);
                        payload.put_u8(crate::pvdata::encode::TAG_NULL);
                        true
                    }
                    Ok(RpcReply::Value(resp_desc, resp_value)) => {
                        Status::ok().write_into(order, &mut payload);
                        // Spawned task cannot hold &mut EncodeTypeCache; use inline
                        // encode_type_desc (no cache) for RPC responses.
                        encode_type_desc(&resp_desc, order, &mut payload);
                        encode_pv_field(&resp_value, &resp_desc, order, &mut payload);
                        true
                    }
                    Err(e) => {
                        e.wire_status().write_into(order, &mut payload);
                        false
                    }
                };
                let h =
                    PvaHeader::application(true, order, Command::Rpc.code(), payload.len() as u32);
                let mut buf = Vec::new();
                h.write_into(&mut buf);
                buf.extend_from_slice(&payload);
                if replied_ok {
                    exec_fin_guard.mark_success();
                }
                let _ = tx_clone.send(buf).await;
            });
            finish_exec_data_task(ch, ioid, subcmd, abort);
        }
        // PUT_GET / PROCESS / GET_FIELD have dedicated handlers
        // (`handle_put_get`, `handle_process`, `handle_get_field`) and are
        // never dispatched into `handle_op`.
        OpKind::PutGet | OpKind::Process | OpKind::Array | OpKind::GetField => {
            unreachable!(
                "PUT_GET / PROCESS / ARRAY / GET_FIELD are routed to their own handlers, not handle_op"
            )
        }
    }
    Ok(())
}

/// Returns `Some(AbortOnDrop)` when a slow-path task was spawned so the
/// caller can store it for connection-lifetime abort on teardown.
async fn handle_get_field(
    frame: &Frame,
    tx: &SrvTx,
    channels: &mut HashMap<u32, ChannelState>,
    order: ByteOrder,
    peer: SocketAddr,
    cred: &ClientCredentials,
    // data-phase-completion sender (see [`handle_op`]). The slow-path
    // introspection task installs an `ExecFinishGuard` so the read-loop
    // owner releases the reserved GET_FIELD IOID once the reply is sent
    // (or the task is aborted by DESTROY / teardown).
    exec_fin_tx: &mpsc::UnboundedSender<ExecFinished>,
) -> PvaResult<()> {
    // Inbound payload decodes with the frame's own header order (pvxs
    // latches `peerBE` per received message, conn.cpp:195-198); `order`
    // (config) is used only for outbound reply frames.
    let inbound_order = frame.order();
    let mut cur = frame.cursor();
    let sid = cur
        .get_u32(inbound_order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let ioid = cur
        .get_u32(inbound_order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let _sub = crate::proto::decode_string(&mut cur, inbound_order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;

    // pvxs serverintrospect.cpp:159 silently returns on
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
    // pvxs rejects GET_FIELD when the IOID is already live on the
    // connection (`opByIOID.find(ioid)!=end()`, serverintrospect.cpp:157).
    // A reserved slow-path GET_FIELD op lives in `ch.ops` too (below), so a
    // duplicate GET_FIELD frame for an in-flight introspection is caught by
    // this same check rather than spawning a second task that double-replies.
    if ioid_live_on_conn(channels, ioid) {
        debug!(
            sid,
            ioid, "GET_FIELD reuses IOID already live on connection: dropping (pvxs parity)"
        );
        return Ok(());
    }
    // Attribute the GET_FIELD request and its reply to the channel
    // (pvxs serverintrospect.cpp:164 `chan->statRx += rxlen`, :45
    // `ch->statTx += enqueueTxBody(CMD_GET_FIELD)`); both the cached
    // fast-path reply and the spawned slow-path reply go through
    // `chan_tx`.
    chan.stat.add_op_rx(frame);
    let chan_tx = ChannelTx::new(tx.clone(), chan.stat.clone());
    if let Some(intro) = chan.introspection.clone() {
        // Fast path: introspection already cached on the channel; reply
        // inline. This completes synchronously before returning to the read
        // loop, so there is no async window for a duplicate IOID to race —
        // pvxs inserts+removes the introspect op within one `doReply` here
        // too. No reservation needed.
        let mut payload = Vec::new();
        payload.put_u32(ioid, order);
        Status::ok().write_into(order, &mut payload);
        encode_type_desc(&intro, order, &mut payload);
        let h = PvaHeader::application(true, order, Command::GetField.code(), payload.len() as u32);
        let mut buf = Vec::new();
        h.write_into(&mut buf);
        buf.extend_from_slice(&payload);
        let _ = chan_tx.send(buf).await;
        return Ok(());
    }

    // Slow path: introspection not yet cached; fetch from the
    // CREATE_CHANNEL-bound owner without blocking the read loop — not the
    // top-level registry (pvxs serverchan.cpp:70-112 / server.cpp:100-112;
    // see `handle_op`).
    let pv_name = chan.name.clone();
    let src = chan.source.clone();
    let tx_clone = chan_tx.clone();
    // introspect under the downstream connection's identity. pvxs builds
    // the GET_FIELD ConnectOp with `conn->cred` (`serverintrospect.cpp:66`);
    // a gateway must resolve the upstream type against THIS peer's
    // credentials. `pv_request` is `None` — GET_FIELD carries no pvRequest.
    let conn_ctx = crate::server_native::source::ChannelContext {
        peer,
        account: cred.account.clone(),
        method: cred.method.clone(),
        host: cred.host.clone(),
        authority: cred.authority.clone(),
        roles: cred.roles.clone(),
        pv_request: None,
        log: Default::default(),
    };
    // The immutable borrow of `chan` ends here (all needed values cloned),
    // so we can take the mutable borrow to reserve the IOID.

    // Reserve the IOID as a real GET_FIELD op before spawning, mirroring
    // pvxs setting the `ServerIntrospect` op to Executing in `opByIOID`
    // (serverintrospect.cpp:164-178). The op carries no descriptor yet
    // (that is what the introspection fetches); `last_request = true` so the
    // `ExecFinished` owner removes it once the single reply is out
    // (apply_exec_finish), and `data_task_abort` (installed below) lets
    // DESTROY_REQUEST / channel teardown abort the in-flight task by
    // dropping the op — the same lifecycle GET/PUT/RPC use.
    let mut reserve = non_monitor_op_state(
        Arc::new(FieldDesc::Variant),
        OpKind::GetField,
        BitSet::with_capacity(0),
    );
    reserve.exec_state = ExecState::Executing;
    reserve.last_request = true;
    let op_id = reserve.monitor_op_id;
    let chan_mut = channels.get_mut(&sid).expect("SID presence verified above");
    chan_mut.ops.insert(ioid, reserve);

    let exec_fin = ExecFinished {
        sid,
        ioid,
        op_id,
        success: false,
    };
    let exec_fin_tx_task = exec_fin_tx.clone();
    let abort = poll_inline_or_spawn(async move {
        // terminal finalizer — releases the reserved IOID on EVERY exit
        // (reply sent, panic, or abort), like the GET/PUT/RPC exec tasks.
        let _exec_fin_guard = ExecFinishGuard {
            tx: exec_fin_tx_task,
            fin: exec_fin,
        };
        let intro = src.get_introspection_checked(&pv_name, conn_ctx).await;
        let mut payload = Vec::new();
        payload.put_u32(ioid, order);
        match intro {
            Some(desc) => {
                // pvxs `serverintrospect.cpp:38-42`: `ioid + status`
                // then the descriptor, written only when non-null.
                Status::ok().write_into(order, &mut payload);
                encode_type_desc(&desc, order, &mut payload);
            }
            None => {
                // A source that cannot supply a descriptor must reply
                // `Status::Error` with NO descriptor — pvxs
                // `ServerIntrospectControl::error` →
                // `doReply(nullptr, Status::Error)`
                // (`serverintrospect.cpp:83-87`), and the `if(type)`
                // guard at `:41-42` omits the type word. Fabricating a
                // `Variant` here (the old `unwrap_or`) reported success
                // and taught the client a wrong type tree, so a later
                // GET/PUT/MONITOR would decode against the wrong shape.
                Status::error("field introspection unavailable".to_string())
                    .write_into(order, &mut payload);
            }
        }
        let h = PvaHeader::application(true, order, Command::GetField.code(), payload.len() as u32);
        let mut buf = Vec::new();
        h.write_into(&mut buf);
        buf.extend_from_slice(&payload);
        let _ = tx_clone.send(buf).await;
    });
    // Install the abort guard on the reserved op so DESTROY_REQUEST /
    // teardown (which drop the op) cancel this task. `subcmd` is irrelevant
    // here — `last_request` is already set on the reserved op above.
    finish_exec_data_task(chan_mut, ioid, 0, abort);
    Ok(())
}

async fn send_op_error(
    tx: &SrvTx,
    kind: OpKind,
    ioid: u32,
    // the reply's sub-command byte. pvxs writes the operation's
    // current subcmd into EVERY GET/PUT/RPC reply (`serverget.cpp:82-84`),
    // recording the data-phase subcmd on the op before the callback runs
    // (`serverget.cpp:475`). An error therefore preserves the request's
    // phase: an INIT-negotiation failure echoes the INIT subcmd (`0x08`),
    // a data-phase failure echoes the request's data subcmd (`0x00` for a
    // GET exec, `0x40` for a PUT readback). Every caller passes its
    // in-scope request `subcmd`, exactly as the success-reply paths do
    // (`payload.put_u8(subcmd)`). Hardcoding `0x08` here mis-framed every
    // data-phase error as an INIT response, so a client waiting for GET
    // data saw an unexpected INIT instead of the failure status.
    subcmd: u8,
    status: Status,
    order: ByteOrder,
) -> PvaResult<()> {
    let buf = build_op_error_frame(kind, ioid, subcmd, status, order);
    let _ = tx.send(buf).await;
    Ok(())
}

/// Per-channel op error reply. pvxs charges every error reply to the
/// owning channel's counter too (the reply leaves via the same
/// `enqueueTxBody` → `chan->statTx` primitive as the success reply),
/// so any error path on a resolved channel must go through `chan_tx`
/// — the unknown-SID path has no channel and uses [`send_op_error`].
async fn send_chan_op_error(
    chan_tx: &ChannelTx,
    kind: OpKind,
    ioid: u32,
    subcmd: u8,
    status: Status,
    order: ByteOrder,
) -> PvaResult<()> {
    let buf = build_op_error_frame(kind, ioid, subcmd, status, order);
    let _ = chan_tx.send(buf).await;
    Ok(())
}

/// Encode a single op error reply frame (`kind` command, `ioid`,
/// `subcmd`, error `Status`). Shared by the connection-level
/// [`send_op_error`] and the per-channel [`send_chan_op_error`] so the
/// two cannot drift on framing.
fn build_op_error_frame(
    kind: OpKind,
    ioid: u32,
    subcmd: u8,
    status: Status,
    order: ByteOrder,
) -> Vec<u8> {
    let cmd = kind.command();
    let mut payload = Vec::new();
    payload.put_u32(ioid, order);
    payload.put_u8(subcmd);
    status.write_into(order, &mut payload);
    let h = PvaHeader::application(true, order, cmd.code(), payload.len() as u32);
    let mut buf = Vec::new();
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    buf
}

/// Encode a single `CMD_MESSAGE` frame (`ioid`, `messageType`,
/// `message`). pvxs `ServerConn::logRemote()` (`serverconn.cpp:146-160`)
/// sends operation diagnostics this way. The payload layout mirrors the
/// client decoder `client_native::server_conn` (and is
/// `ioid:u32 + mtype:u8 + message:string`); the client logs `Warning`
/// (1) at `warn!` and `Fatal` (3) at `error!`. Used by the MONITOR INIT
/// owner to surface [`MonitorOptionDiag`] negotiation diagnostics, and
/// by [`flush_remote_log`] to surface source-layer ones.
fn build_message_frame(ioid: u32, level: MessageType, msg: &str, order: ByteOrder) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.put_u32(ioid, order);
    payload.put_u8(level as u8);
    encode_string_into(msg, order, &mut payload);
    let h = PvaHeader::application(true, order, Command::Message.code(), payload.len() as u32);
    let mut buf = Vec::new();
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    buf
}

/// Drain a source's per-operation [`RemoteLog`](crate::server_native::source::RemoteLog) onto the wire as
/// IOID-tagged `CMD_MESSAGE` frames — the Rust half of pvxs
/// `RemoteLogger::logRemote()` (`serverconn.cpp:146-160`), which the
/// pvxs IOC source layer uses for "present but unusable option"
/// diagnostics (`ioc/groupsource.cpp:560`, `ioc/singlesource.cpp:129`,
/// `ioc/iocsource.cpp:447`).
///
/// The single owner of that transition: a source records, the
/// connection emits. Called immediately after EVERY `ChannelSource` op
/// call that carries an IOID and BEFORE that op's reply frame is
/// enqueued, so a diagnostic always precedes the reply it qualifies —
/// the order pvxs produces, where `logRemote` runs inside the source
/// callback and the reply is sent when it returns.
async fn flush_remote_log(
    log: &crate::server_native::source::RemoteLog,
    ioid: u32,
    order: ByteOrder,
    tx: &ChannelTx,
) {
    for m in log.take() {
        if tx
            .send(build_message_frame(ioid, m.level, &m.message, order))
            .await
            .is_err()
        {
            return;
        }
    }
}

#[allow(unused_imports)]
use crate::proto::ReadExt;
const _: u8 = PVA_VERSION;

/// **The one wire changed-bitset rule**, shared by every framed value the
/// server sends — MONITOR data, the MONITOR seed, the GET reply, the PUT_GET
/// readback. pvxs frames all of them with the identical call,
/// `to_wire_valid(R, value, &pvMask)` (`servermon.cpp:174`,
/// `serverget.cpp:104`): the leaves the source ASSIGNED, intersected with the
/// request mask.
///
/// * `marked = Some(paths)` — the source declared what it assigned. Used by
///   the QSRV sources: a group monitor marks its `+trigger` targets
///   (`groupsource.cpp:288`, assigned-not-changed, so an unchanged leaf still
///   carries), and a read marks what `IOCSource::initialize` + `IOCSource::get`
///   actually wrote (`getProperties` never assigns `control.minStep`,
///   `valueAlarm.active`, the four `valueAlarm.*Severity` leaves or
///   `valueAlarm.hysteresis`).
/// * `marked = None` — a wholly-assigned value (pvxs's fully-marked `Value`):
///   every leaf the request selected.
///
/// `MonitorQueue::real` gates on this same computation, so an admitted post
/// can never frame an empty changed-bitset.
fn read_changed_bitset(intro: &FieldDesc, mask: &BitSet, marked: Option<&[String]>) -> BitSet {
    match marked {
        Some(paths) => crate::pvdata::encode::marked_wire_changed_bitset(intro, paths, mask),
        // `mask` is a *selection* mask (request_to_mask) whose structure bits
        // are permission bits — canonicalize it into pvxs's leaf-enumerated
        // wire form (`to_wire_valid`, dataencode.cpp:414-439).
        None => crate::pvdata::encode::canonical_changed_bitset(intro, mask),
    }
}

/// Build a complete MONITOR data frame (header + payload) for a single value
/// emission. Pulled out so the back-pressure squashing loop can call it.
fn build_monitor_payload(
    ioid: u32,
    intro: &FieldDesc,
    value: &PvField,
    marked: Option<&[String]>,
    mask: &BitSet,
    order: ByteOrder,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.put_u32(ioid, order);
    payload.put_u8(0x00);
    // PVA monitor data: changed bitset + partial value + overrun bitset.
    let changed = read_changed_bitset(intro, mask, marked);
    changed.write_into(order, &mut payload);
    crate::pvdata::encode::encode_pv_field_with_bitset(
        value,
        intro,
        &changed,
        0,
        order,
        &mut payload,
    );
    // pvxs `servermon.cpp:174-176` always writes a single 0x00 (empty
    // BitSet) for the trailing overrun mask — `// TODO: placeholder for
    // overrun mask` — regardless of any server-side squash. Emit that
    // exact wire form: a server-computed overrun set makes a pvxs client
    // set `servSquash=true` and bump `nSrvSquash` (clientmon.cpp:554-564),
    // a counter that stays 0 against a real pvxs server.
    BitSet::new().write_into(order, &mut payload);
    let h = PvaHeader::application(true, order, Command::Monitor.code(), payload.len() as u32);
    let mut buf = Vec::with_capacity(8 + payload.len());
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    buf
}

/// coalesce two cooked monitor updates when the server
/// squashes events under back-pressure or pause. The newer value wins;
/// the marked-leaf sets union so the emitted frame still marks every
/// field that changed across the coalesced burst. A `None` on either
/// side means "wholly assigned — every leaf the request selected", which
/// over-marks safely, so the union of `None` with anything stays `None`.
///
/// The squash also records OVERRUN in [`crate::server_native::MonitorUpdate`]:
/// the dropped intermediate's distinct values are lost, so every leaf changed
/// in BOTH the dropped older and the surviving newer update is added to the
/// result's overrun set, and the two updates' own overrun sets union forward —
/// pva2pva `moncache.cpp:160-168`.
///
/// That set does NOT reach the cooked wire, and must not: pvxs's server writes
/// a hard-empty overrun bitset on every MONITOR data frame
/// (`servermon.cpp:174-176`, `// TODO: placeholder for overrun mask`), so every
/// cooked builder here writes one too — a server-computed overrun set makes a
/// pvxs client set `servSquash` and bump `nSrvSquash` (`clientmon.cpp:554-564`),
/// a counter that stays 0 against a real pvxs server. The only overrun bits the
/// port puts on the wire are the ones the RAW forwarder decoded from an
/// UPSTREAM server's frame (`build_raw_monitor_frame`), which it must carry
/// through unchanged.
///
/// A `type_changed` boundary must SURVIVE the squash: once the upstream
/// descriptor changed, no value (squashed-old or post-boundary-new) may
/// be delivered under the negotiated descriptor, so if either side is a
/// boundary the result is the boundary — the dispatch loop then emits
/// MONITOR FINISH instead of encoding a stale-descriptor value. This is
/// what keeps the decoded type-change marker from being lost when the
/// bounded FIFO overflow ([`push_squash_monitor`]) coalesces a burst.
fn coalesce_monitor_update(
    older: crate::server_native::MonitorUpdate,
    newer: crate::server_native::MonitorUpdate,
) -> crate::server_native::MonitorUpdate {
    if older.type_changed || newer.type_changed {
        return crate::server_native::MonitorUpdate::type_change();
    }
    // pva2pva moncache.cpp:160-168 squash-into-overflow accounting:
    //   overrun |= older.overrun | newer.overrun   (carry both forward)
    //   overrun |= older.changed & newer.changed    (lost intermediate)
    //   changed |= older.changed | newer.changed    (accumulate, below)
    // A leaf is overrun when a distinct value for it was overwritten:
    // it is marked changed in BOTH the dropped `older` update and the
    // surviving `newer` one, so the downstream missed a transition.
    // Computed from the explicit marked sets BEFORE they are moved into
    // the union below; when either side lacks an explicit set the
    // intersection contributes nothing (the leaves cannot be named), but
    // any overrun the producers already recorded still carries forward.
    let mut overrun = older.overrun;
    for p in newer.overrun {
        if !overrun.contains(&p) {
            overrun.push(p);
        }
    }
    if let (Some(a), Some(b)) = (older.marked.as_ref(), newer.marked.as_ref()) {
        for p in a {
            if b.contains(p) && !overrun.contains(p) {
                overrun.push(p.clone());
            }
        }
    }
    let marked = match (older.marked, newer.marked) {
        (Some(mut a), Some(b)) => {
            for p in b {
                if !a.contains(&p) {
                    a.push(p);
                }
            }
            Some(a)
        }
        _ => None,
    };
    crate::server_native::MonitorUpdate {
        value: newer.value,
        marked,
        type_changed: false,
        overrun,
    }
}

/// outcome of turning one [`crate::server_native::RawMonitorEvent`]
/// into a downstream wire frame. Carries the single malformed-raw policy
/// so the same-endian forward and the cross-endian re-encode cannot
/// diverge: a frame that cannot be produced terminates the stream with
/// an error rather than being silently dropped.
enum RawMonitorFrame {
    /// Forward this frame to the downstream subscriber.
    Forward(Vec<u8>),
    /// The raw body could not be re-encoded (malformed/truncated under
    /// the upstream byte order). Emit `frame` (a terminal MONITOR error)
    /// and end the stream; `reason` is for server-side logging.
    Terminate { frame: Vec<u8>, reason: String },
}

/// build the downstream wire frame for a single raw monitor
/// event, owning the malformed-body policy in one place.
///
/// - Same-endian: forward the body verbatim (zero-copy memcpy via
///   [`build_monitor_payload_raw`]). A malformed body is carried through
///   so the downstream client fails at its own protocol boundary.
/// - Cross-endian: decode under the upstream order and re-encode under
///   `downstream_order` ([`reencode_raw_monitor`]). A malformed body
///   cannot be re-encoded, so it yields [`RawMonitorFrame::Terminate`]
///   with a MONITOR error frame — pvxs likewise resets the connection
///   when a monitor message is not good (`clientmon.cpp:596`). Earlier
///   code dropped it with a debug log + `continue`, hiding upstream
///   corruption and keeping the monitor alive as if the bad update never
///   existed — and making cross-endian behaviour disagree with the
///   same-endian forward on identical input.
fn raw_monitor_frame(
    ioid: u32,
    intro: &FieldDesc,
    ev: &crate::server_native::RawMonitorEvent,
    downstream_order: ByteOrder,
) -> RawMonitorFrame {
    if ev.byte_order != downstream_order {
        match reencode_raw_monitor(ioid, intro, ev, downstream_order) {
            Ok(p) => RawMonitorFrame::Forward(p),
            Err(reason) => RawMonitorFrame::Terminate {
                frame: build_monitor_error(
                    ioid,
                    &format!("raw monitor re-encode failed: {reason}"),
                    downstream_order,
                ),
                reason,
            },
        }
    } else {
        RawMonitorFrame::Forward(build_monitor_payload_raw(ioid, ev, downstream_order))
    }
}

/// decode a raw MONITOR event captured under upstream
/// byte-order and re-encode it under the downstream connection's
/// byte-order. Used when a gateway forwards raw events between
/// peers with different negotiated byte orders.
///
/// Body layout (pvxs `servermon.cpp:159-178`): `changed bitset |
/// partial value | overrun bitset`. Each leaf scalar in the
/// partial value is byte-order-sensitive, so a memcpy-forward like
/// the fast path would deliver mis-decoded numbers to the downstream
/// peer; decode-and-re-encode is the only correct path.
fn reencode_raw_monitor(
    ioid: u32,
    intro: &FieldDesc,
    ev: &crate::server_native::RawMonitorEvent,
    downstream_order: ByteOrder,
) -> Result<Vec<u8>, String> {
    let mut cur = std::io::Cursor::new(&ev.body_bytes[..]);
    let changed = BitSet::decode(&mut cur, ev.byte_order)
        .map_err(|e| format!("decode changed bitset: {e}"))?;
    let value = crate::pvdata::encode::decode_pv_field_with_bitset(
        intro,
        &changed,
        0,
        &mut cur,
        ev.byte_order,
    )
    .map_err(|e| format!("decode value with bitset: {e}"))?;
    // the overrun bitset is part of the MONITOR DATA wire format,
    // NOT optional. pvxs reads it unconditionally
    // (`clientmon.cpp:550` `from_wire(M, overrun)`) and disconnects when
    // the message is not good afterwards (`clientmon.cpp:596`). A failed
    // decode here means a truncated/corrupt upstream body; defaulting to
    // an empty bitset would fabricate a valid frame from corruption and
    // erase the server-squash overrun signal — and make a same-endian
    // forward (which carries the malformed bytes through verbatim, so
    // the downstream client detects it) and a cross-endian re-encode
    // disagree on the same event. Propagate the error so the caller can
    // tear the monitor down instead.
    let overrun = BitSet::decode(&mut cur, ev.byte_order)
        .map_err(|e| format!("decode overrun bitset: {e}"))?;

    let mut payload = Vec::new();
    payload.put_u32(ioid, downstream_order);
    payload.put_u8(0x00);
    changed.write_into(downstream_order, &mut payload);
    crate::pvdata::encode::encode_pv_field_with_bitset(
        &value,
        intro,
        &changed,
        0,
        downstream_order,
        &mut payload,
    );
    overrun.write_into(downstream_order, &mut payload);

    let h = PvaHeader::application(
        true,
        downstream_order,
        Command::Monitor.code(),
        payload.len() as u32,
    );
    let mut buf = Vec::with_capacity(8 + payload.len());
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Raw-frame variant: build a MONITOR data frame from a
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

/// build a MONITOR error frame — subcmd `0x10` (finish) plus
/// a non-success `Status`. Used when a server-side transformation
/// filter produces a value that cannot be represented in the
/// monitor's negotiated wire descriptor; the stream ends with an
/// explicit error rather than silently emitting a wrong value. pvxs
/// `servermon.cpp:178` notes the finish frame "could be used to send
/// an error".
fn build_monitor_error(ioid: u32, msg: &str, order: ByteOrder) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.put_u32(ioid, order);
    payload.put_u8(0x10);
    Status::error(msg.to_string()).write_into(order, &mut payload);
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

/// A fixed-order outbound cell for `handle_op` calls in tests that do not
/// renegotiate the connection byte order mid-stream. Production threads the
/// read loop's live cell; these tests latch it once to the handler's `order`
/// so a spawned MONITOR task reads the same order it was given. Defined at
/// module level so every `#[cfg(test)]` sub-module reaches it via `super::*`.
#[cfg(test)]
fn fixed_out_order(order: ByteOrder) -> Arc<std::sync::atomic::AtomicBool> {
    Arc::new(std::sync::atomic::AtomicBool::new(order.is_big()))
}

/// The peer every credentials-constructing test is reached from. A
/// documentation range (RFC 5737 TEST-NET-3), deliberately NOT the loopback
/// or any name a wire `host` string in these tests uses, so a wire value
/// leaking into `ClientCredentials::host` is visible rather than
/// coincidentally equal. Defined at module level so every `#[cfg(test)]`
/// sub-module reaches it via `super::*`. Nothing binds it.
#[cfg(test)]
const TEST_PEER: std::net::SocketAddr = std::net::SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(198, 51, 100, 7)),
    44321,
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{OpResponse, decode_op_response, try_parse_frame};
    use crate::pvdata::{PvStructure, ScalarType, ScalarValue};
    use crate::server_native::MonitorStream;

    /// Every task this file spawns for a *connection* — the writer, the MONITOR
    /// subscriber, the CREATE_CHANNEL resolver, and the seven data-phase
    /// execs — goes through `runtime::task::spawn`, not `tokio::spawn`
    /// (RTEMS phase 6 item 5, stage 2). A bare `tokio::spawn` panics on a
    /// thread with no tokio runtime, which is exactly the thread the blocking
    /// driver will run a connection on — and it panics at *runtime*, on the
    /// target, not here. So pin it as source inspection: production scope
    /// must contain no `tokio::spawn` at all.
    ///
    /// Scope is this file, and after the accept loop moved to
    /// [`super::super::accept`] (item 7 stage A) that is exactly the
    /// connection scope the name claims. The rule is not *relaxed* for the
    /// accept module — `accept.rs` carries the same zero-bare-`tokio::spawn`
    /// assertion in its own tests, because its per-connection task is a
    /// `JoinSet` method (`conn_tasks.spawn`), not this literal. What
    /// `accept.rs` is additionally allowed, and this file is not, is the
    /// non-spawn tokio surface a host socket driver needs: `TcpListener`, the
    /// two TLS handshake deadlines, the accept-error backoff. Those are item
    /// 7's to replace wholesale with a blocking driver, so pinning them here
    /// would pin the wrong thing.
    #[test]
    fn connection_scope_spawns_go_through_the_runtime_seam() {
        let src = include_str!("tcp.rs");
        // Production scope ends at the first column-0 `#[cfg(test)]`.
        let prod = match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        // Fail closed: an earlier `#[cfg(test)]` helper must not shrink the
        // slice past the connection handler and make this pass vacuously.
        assert!(
            prod.contains("async fn handle_connection_io"),
            "production slice no longer covers the connection handler"
        );
        // Written split so this assertion cannot match its own source text.
        let literal = concat!("tokio", "::spawn(");
        let hits = prod.matches(literal).count();
        assert_eq!(
            hits, 0,
            "production scope must spawn through `runtime::task::spawn`; \
             found {hits} bare `{literal}`"
        );
    }

    /// A1, structural: `ClientCredentials::host` is the string the ACF
    /// `HAG(...)` gate matches, and this file is the only place in the
    /// workspace that writes it. Pin the two properties that make a wire
    /// value *unable* to reach it, rather than merely corrected afterwards:
    ///
    /// 1. Production scope holds exactly ONE write, and it is the funnel's
    ///    `self.host = Self::acf_host_from_peer(peer)`.
    /// 2. `parse_client_credentials` — the one function that reads the
    ///    CONNECTION_VALIDATION body — has no `("host", ...)` decode arm and
    ///    hands back credentials only through `with_server_derived`.
    ///
    /// Mutation-provable: re-add the deleted `("host", …)` arm, or make the
    /// parser return `Ok(Some(creds))` un-funnelled, and this fails.
    /// Comment lines are stripped first so the NOTE explaining the absent arm
    /// does not satisfy the check it documents.
    #[test]
    fn the_acf_host_is_written_only_by_the_peer_funnel() {
        let src = include_str!("tcp.rs");
        let prod = match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        };
        assert!(
            prod.contains("fn parse_client_credentials"),
            "production slice no longer covers the credentials parser"
        );
        let code = |s: &str| -> String {
            s.lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let prod_code = code(prod);
        let writes: Vec<&str> = prod_code
            .lines()
            .filter(|l| l.contains(".host =") || l.contains(".host="))
            .collect();
        assert_eq!(
            writes,
            vec!["        self.host = Self::acf_host_from_peer(peer);"],
            "the ACF host must be written only by `with_server_derived`"
        );

        let start = prod
            .find("fn parse_client_credentials")
            .expect("checked above");
        let end = prod[start..]
            .find("\n/// Type-erased read/write halves")
            .expect("parser is followed by the SrvRead/SrvWrite aliases")
            + start;
        let parser = code(&prod[start..end]);
        assert!(
            !parser.contains("\"host\""),
            "the CONNECTION_VALIDATION parser must have no `host` decode arm"
        );
        let returns: Vec<&str> = parser
            .lines()
            .filter(|l| l.contains("Ok(Some("))
            .map(|l| l.trim())
            .collect();
        assert_eq!(
            returns,
            vec!["Ok(Some(creds.with_server_derived(peer)))"],
            "every credential the parser hands back must pass the funnel"
        );
    }

    /// a throwaway MONITOR-completion sender for `handle_op`
    /// calls in tests that do not exercise the read-loop owner's removal
    /// arm. The receiver is dropped immediately, so the spawned subscriber
    /// task's `MonitorFinishGuard::drop` `send` is a harmless no-op (the
    /// op stays in `ch.ops` exactly as before this fix, since these tests
    /// never call [`apply_monitor_finish`]). Tests that DO assert the
    /// cleanup build a real channel and keep the receiver.
    fn discard_mon_fin() -> mpsc::UnboundedSender<MonitorFinished> {
        mpsc::unbounded_channel().0
    }

    /// a throwaway data-phase-completion sender for handler calls in
    /// tests that do not exercise the read-loop owner's `Idle`-return arm.
    /// The receiver is dropped immediately, so the spawned task's
    /// `ExecFinishGuard::drop` `send` is a harmless no-op (the op keeps its
    /// `Executing` state, exactly as a single in-flight EXEC would, since
    /// these tests never call [`apply_exec_finish`]). Tests that assert the
    /// return-to-`Idle` build a real channel and keep the receiver.
    fn discard_exec_fin() -> mpsc::UnboundedSender<ExecFinished> {
        mpsc::unbounded_channel().0
    }

    /// `cross_watermark` is the primitive that
    /// closes the residual pause/resume reorder. Tested by invariant
    /// boundary, not by narrative — it must fire once per real crossing
    /// (`None` when already in the requested state), encode the
    /// resulting state in its PARITY (odd = above/resume, even =
    /// below/pause), and mint a STRICTLY-monotonic token across
    /// crossings. Together these let the gateway order pause/resume by
    /// the token regardless of which firing task reaches it first — a
    /// resume (higher token) can never be lost behind an earlier pause.
    #[test]
    fn fr11_cross_watermark_is_once_per_crossing_parity_and_monotonic() {
        use std::sync::atomic::AtomicU64;
        // Starts odd (1) = window above high (matches OpState init).
        let state = AtomicU64::new(1);

        // Already above → HIGH is a no-op (no double-fire).
        assert_eq!(cross_watermark(&state, true), None, "already above");

        // Above → below (LOW): fires once, mints an EVEN token.
        let low1 = cross_watermark(&state, false).expect("LOW crosses");
        assert_eq!(low1 % 2, 0, "below crossing token is even");
        assert_eq!(cross_watermark(&state, false), None, "already below");

        // Below → above (HIGH): fires once, mints an ODD token strictly
        // greater than the previous.
        let high1 = cross_watermark(&state, true).expect("HIGH crosses");
        assert_eq!(high1 % 2, 1, "above crossing token is odd");
        assert!(high1 > low1, "token strictly monotonic across crossings");

        // Another full LOW→HIGH cycle keeps growing monotonically.
        let low2 = cross_watermark(&state, false).expect("LOW crosses again");
        let high2 = cross_watermark(&state, true).expect("HIGH crosses again");
        assert!(
            low2 > high1 && high2 > low2,
            "tokens stay strictly monotonic: {low1} < {high1} < {low2} < {high2}"
        );
    }

    /// the withdraw-on-teardown finalizer
    /// ([`WatermarkWithdrawOnDrop`]) closes the cross-op strand — a monitor
    /// op destroyed while it held its *shared* upstream paused must
    /// withdraw its vote when its subscriber task drops, or it can starve
    /// co-subscribers that share the upstream entry. Tested by the
    /// invariant: dropping the guard fires exactly one
    /// [`WatermarkKind::Withdraw`](crate::server_native::source::WatermarkKind::Withdraw) carrying this op's `op_id` (the gateway
    /// then removes the op's vote and recomputes the aggregate). Firing is
    /// unconditional — a torn-down op always withdraws — and op-scoped, so
    /// it cannot disturb a co-subscriber's vote.
    #[test]
    fn fr11_withdraw_on_drop_fires_op_scoped_withdraw() {
        use crate::server_native::source::{WatermarkEvent, WatermarkKind};

        struct RecordingWmSource {
            events: Arc<parking_lot::Mutex<Vec<(u64, WatermarkKind)>>>,
        }
        impl crate::server_native::source::ChannelSource for RecordingWmSource {
            async fn list_pvs(&self) -> Vec<String> {
                vec!["WM:PV".into()]
            }
            async fn has_pv(&self, name: &str) -> bool {
                name == "WM:PV"
            }
            async fn get_introspection(&self, _name: &str) -> Option<FieldDesc> {
                Some(FieldDesc::Variant)
            }
            async fn get_value(&self, _name: &str) -> Option<PvField> {
                Some(PvField::Scalar(ScalarValue::Double(1.0)))
            }
            async fn put_value(&self, _name: &str, _value: PvField) -> Result<(), OpError> {
                Ok(())
            }
            async fn is_writable(&self, _name: &str) -> bool {
                false
            }
            async fn subscribe(
                &self,
                _name: &str,
            ) -> Option<crate::server_native::MonitorStream<PvField>> {
                None
            }
            fn notify_watermark(
                &self,
                _name: &str,
                _ctx: &crate::server_native::source::ChannelContext,
                ev: WatermarkEvent,
            ) {
                self.events.lock().push((ev.op_id, ev.kind));
            }
        }

        let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let src: DynSource = Arc::new(RecordingWmSource {
            events: events.clone(),
        });
        let ctx = crate::server_native::source::ChannelContext {
            peer: "127.0.0.1:9001".parse().unwrap(),
            account: String::new(),
            method: "anonymous".into(),
            host: String::new(),
            authority: String::new(),
            roles: Vec::new(),
            pv_request: None,
            log: Default::default(),
        };

        // Dropping the guard fires exactly one Withdraw scoped to op 42.
        {
            let _g = WatermarkWithdrawOnDrop {
                src: src.clone(),
                pv_name: "WM:PV".into(),
                ctx: ctx.clone(),
                op_id: 42,
            };
        }
        assert_eq!(
            *events.lock(),
            vec![(42, WatermarkKind::Withdraw)],
            "teardown fires one op-scoped Withdraw, unconditionally"
        );

        // A second op's guard withdraws its OWN id only — votes never
        // cross between co-subscribers.
        events.lock().clear();
        {
            let _g = WatermarkWithdrawOnDrop {
                src: src.clone(),
                pv_name: "WM:PV".into(),
                ctx: ctx.clone(),
                op_id: 99,
            };
        }
        assert_eq!(
            *events.lock(),
            vec![(99, WatermarkKind::Withdraw)],
            "each op withdraws only its own vote"
        );
    }

    /// a PVA `ulong[]` monitor value must reach the `arr`
    /// server-side filter as `EpicsValue::UInt64Array`. Before the
    /// fix `crate::leaf_convert::pv_leaf_to_epics_value`'s `array`
    /// helper had no `ULong` arm, so a wire-decoded
    /// `ScalarArrayTyped::ULong` fell through to `None`;
    /// `pv_field_to_filter_event` then substituted a scalar
    /// `Double(0.0)`, and a filtered `DBF_UINT64` waveform was
    /// emitted as an empty `ulong[]` payload.
    #[test]
    fn pf_r1_ulong_array_monitor_value_reaches_filter_as_uint64array() {
        use crate::pvdata::TypedScalarArray;
        use epics_base_rs::types::EpicsValue;

        let big = (i64::MAX as u64) + 5;
        let typed = PvField::ScalarArrayTyped(TypedScalarArray::ULong(std::sync::Arc::from(
            vec![big, 2u64].as_slice(),
        )));
        assert_eq!(
            crate::leaf_convert::pv_leaf_to_epics_value(&typed),
            Some(EpicsValue::UInt64Array(vec![big, 2])),
            "ulong[] monitor value must convert to UInt64Array, not fall through to None",
        );
    }

    /// Finding (High): the `_filter` bridge must never fabricate or
    /// coerce a value-leaf whose type differs from the negotiated
    /// monitor descriptor. Two fail-closed boundaries — forward
    /// (inbound leaf has no faithful `EpicsValue`) and backward (a
    /// filter rewrote the leaf to a different type) — both yield
    /// `DescriptorMismatch` (monitor error) instead of corrupted data.
    /// Tested by invariant boundary, not by scenario.
    mod filter_bridge_fail_closed {
        use super::*;
        use epics_base_rs::server::database::filters::{FilterChain, parse_filter_chain};

        fn nt_scalar_value(sv: ScalarValue) -> PvField {
            let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
            s.fields.push(("value".into(), PvField::Scalar(sv)));
            PvField::Structure(s)
        }

        fn nt_scalar_desc(st: ScalarType) -> FieldDesc {
            FieldDesc::Structure {
                struct_id: "epics:nt/NTScalar:1.0".into(),
                fields: vec![("value".into(), FieldDesc::Scalar(st))],
            }
        }

        fn is_mismatch(o: MonitorFilterOutcome) -> bool {
            matches!(o, MonitorFilterOutcome::DescriptorMismatch)
        }

        // --- Forward fail-closed: unsupported scalar leaf -> None. ---

        /// PVA scalar types with no DBR `EpicsValue` counterpart must
        /// fail closed at the forward bridge (None), not be substituted
        /// with `Double(0.0)`.
        #[test]
        fn forward_unsupported_scalars_return_none() {
            // `Byte` is NOT here: it is DBF_CHAR (signed), a faithful leaf
            // carried as `EpicsValue::Char` — see
            // `forward_char_and_uchar_bytes_carry_faithfully`.
            for sv in [
                ScalarValue::Boolean(true),
                ScalarValue::UShort(7),
                ScalarValue::UInt(9),
            ] {
                assert!(
                    pv_field_to_filter_event(&PvField::Scalar(sv.clone())).is_none(),
                    "unsupported scalar {sv:?} must fail closed (None), not fabricate a value",
                );
            }
        }

        /// Supported scalar types still convert (the fix must not
        /// regress the faithful path).
        #[test]
        fn forward_supported_scalars_return_some() {
            for sv in [
                ScalarValue::Double(1.5),
                ScalarValue::Float(2.0),
                ScalarValue::Int(3),
                ScalarValue::Long(4),
                ScalarValue::ULong(5),
                ScalarValue::Short(6),
                ScalarValue::Byte(-56),
                ScalarValue::UByte(7),
                ScalarValue::String("x".into()),
            ] {
                assert!(
                    pv_field_to_filter_event(&PvField::Scalar(sv.clone())).is_some(),
                    "supported scalar {sv:?} must convert faithfully",
                );
            }
        }

        /// Unsupported scalar arrays (uint[]/ushort[]/bool[]/byte[])
        /// fail closed at the forward bridge.
        #[test]
        fn forward_unsupported_arrays_return_none() {
            // `byte[]` is NOT here: it is DBF_CHAR[] (signed), carried as
            // `EpicsValue::CharArray` — see
            // `forward_char_and_uchar_bytes_carry_faithfully`.
            let cases = [
                vec![ScalarValue::UInt(1), ScalarValue::UInt(2)],
                vec![ScalarValue::UShort(1)],
                vec![ScalarValue::Boolean(true)],
            ];
            for items in cases {
                assert!(
                    pv_field_to_filter_event(&PvField::ScalarArray(items.clone())).is_none(),
                    "unsupported array {items:?} must fail closed (None)",
                );
            }
        }

        // --- End-to-end owner: forward fail-closed -> DescriptorMismatch. ---

        /// `NTScalar<Boolean>` under a non-empty chain: the inbound
        /// Boolean leaf has no `EpicsValue`, so the owner emits a
        /// monitor error instead of a fabricated `false`.
        #[test]
        fn owner_boolean_under_filter_is_descriptor_mismatch() {
            let chain = parse_filter_chain(r#"{"ts":{}}"#);
            let value = nt_scalar_value(ScalarValue::Boolean(true));
            let desc = nt_scalar_desc(ScalarType::Boolean);
            assert!(
                is_mismatch(apply_monitor_filter_chain(&chain, &value, &desc)),
                "filtered NTScalar<Boolean> must be a DescriptorMismatch, not a coerced frame",
            );
        }

        /// `NTScalarArray<uint[]>` under a non-empty chain: the inbound
        /// uint[] leaf has no `EpicsValue`, so the owner emits a monitor
        /// error instead of an empty array.
        #[test]
        fn owner_uint_array_under_filter_is_descriptor_mismatch() {
            let chain = parse_filter_chain(r#"{"arr":{}}"#);
            let mut s = PvStructure::new("epics:nt/NTScalarArray:1.0");
            s.fields.push((
                "value".into(),
                PvField::ScalarArray(vec![ScalarValue::UInt(1), ScalarValue::UInt(2)]),
            ));
            let value = PvField::Structure(s);
            let desc = FieldDesc::Structure {
                struct_id: "epics:nt/NTScalarArray:1.0".into(),
                fields: vec![("value".into(), FieldDesc::ScalarArray(ScalarType::UInt))],
            };
            assert!(
                is_mismatch(apply_monitor_filter_chain(&chain, &value, &desc)),
                "filtered uint[] monitor must be a DescriptorMismatch, not an empty array",
            );
        }

        // --- DBF_CHAR (signed byte) round-trips both bridge directions. ---
        //
        // The forward/backward leaf mapping (Byte<->Char signed,
        // UByte<->UChar unsigned) is owned and unit-tested in
        // `crate::leaf_convert` (`dbf_char_is_signed_dbf_uchar_is_unsigned_all_directions`,
        // `forward_is_the_inverse_of_the_serve_backward_mapping`). The
        // end-to-end test below covers the bridge wiring that consumes it.

        /// End-to-end: a filtered DBF_CHAR array monitor must survive the
        /// bridge. Before the Q52 completion an `arr` filter on a `byte[]`
        /// value hit the forward bridge's missing `Byte` arm -> None ->
        /// DescriptorMismatch, terminating the monitor. It must now pass or
        /// transform, re-emitting a signed `byte[]` leaf that fits the
        /// `byte[]` descriptor — never DescriptorMismatch.
        #[test]
        fn owner_char_array_under_arr_filter_is_not_descriptor_mismatch() {
            let chain = parse_filter_chain(r#"{"arr":{}}"#);
            let mut s = PvStructure::new("epics:nt/NTScalarArray:1.0");
            s.fields.push((
                "value".into(),
                PvField::ScalarArray(vec![
                    ScalarValue::Byte(10),
                    ScalarValue::Byte(-56),
                    ScalarValue::Byte(30),
                ]),
            ));
            let value = PvField::Structure(s);
            let desc = FieldDesc::Structure {
                struct_id: "epics:nt/NTScalarArray:1.0".into(),
                fields: vec![("value".into(), FieldDesc::ScalarArray(ScalarType::Byte))],
            };
            match apply_monitor_filter_chain(&chain, &value, &desc) {
                MonitorFilterOutcome::DescriptorMismatch => {
                    panic!("filtered DBF_CHAR[] must not regress to DescriptorMismatch");
                }
                MonitorFilterOutcome::Drop => {
                    panic!("an arr filter must not drop the frame");
                }
                MonitorFilterOutcome::Transformed(PvField::Structure(out)) => {
                    let leaf = out
                        .fields
                        .iter()
                        .find_map(|(k, v)| (k == "value").then_some(v))
                        .expect("transformed frame keeps a value leaf");
                    assert!(
                        matches!(leaf, PvField::ScalarArray(items)
                            if items.iter().all(|x| matches!(x, ScalarValue::Byte(_)))),
                        "sliced DBF_CHAR[] must re-emit a signed byte[] leaf, got {leaf:?}",
                    );
                }
                MonitorFilterOutcome::Transformed(other) => {
                    panic!("transformed frame must stay an NT structure, got {other:?}");
                }
                MonitorFilterOutcome::Pass => {
                    // Acceptable: an identity arr passed the frame unchanged;
                    // the original leaf is already a signed byte[] fitting desc.
                }
            }
        }

        // --- Backward fail-closed: filter changes the leaf type. ---

        /// `ts` in `Seconds` mode rewrites the value leaf to `Int64`.
        /// On an `NTScalar<Double>` that no longer fits the descriptor,
        /// so the owner emits a monitor error rather than coercing the
        /// timestamp `Int64` onto the wire `Double` — the struct_id-only
        /// check would have let it through.
        #[test]
        fn owner_ts_type_change_is_descriptor_mismatch() {
            let chain = parse_filter_chain(r#"{"ts":{"num":"sec"}}"#);
            let value = nt_scalar_value(ScalarValue::Double(42.5));
            let desc = nt_scalar_desc(ScalarType::Double);
            assert!(
                is_mismatch(apply_monitor_filter_chain(&chain, &value, &desc)),
                "ts seconds-mode on NTScalar<Double> must be a DescriptorMismatch",
            );
        }

        /// `ts` in `Generate` mode leaves the value type untouched, so a
        /// faithful `NTScalar<Double>` round-trips and is `Transformed`
        /// (value preserved).
        #[test]
        fn owner_ts_generate_preserves_value() {
            let chain = parse_filter_chain(r#"{"ts":{}}"#);
            let value = nt_scalar_value(ScalarValue::Double(42.5));
            let desc = nt_scalar_desc(ScalarType::Double);
            match apply_monitor_filter_chain(&chain, &value, &desc) {
                MonitorFilterOutcome::Transformed(PvField::Structure(s)) => {
                    assert_eq!(
                        s.get_field("value"),
                        Some(&PvField::Scalar(ScalarValue::Double(42.5))),
                        "value must be preserved through a non-type-changing filter",
                    );
                }
                _ => panic!("expected Transformed(NTScalar<Double>), got a different outcome"),
            }
        }

        /// An empty chain short-circuits to `Pass` even for a value type
        /// the filter engine cannot represent — no conversion is
        /// attempted, so no fail-closed error.
        #[test]
        fn owner_empty_chain_passes_unsupported_type() {
            let chain = FilterChain::new();
            let value = nt_scalar_value(ScalarValue::Boolean(true));
            let desc = nt_scalar_desc(ScalarType::Boolean);
            assert!(
                matches!(
                    apply_monitor_filter_chain(&chain, &value, &desc),
                    MonitorFilterOutcome::Pass
                ),
                "empty chain must Pass without converting the value",
            );
        }

        // --- Backward gate boundaries: transformed_leaf_fits_descriptor. ---

        #[test]
        fn leaf_gate_scalar_type_boundaries() {
            // Matching scalar type fits; differing type does not.
            assert!(transformed_leaf_fits_descriptor(
                &nt_scalar_value(ScalarValue::Double(1.0)),
                &nt_scalar_desc(ScalarType::Double),
            ));
            assert!(!transformed_leaf_fits_descriptor(
                &nt_scalar_value(ScalarValue::Long(1)),
                &nt_scalar_desc(ScalarType::Double),
            ));
        }

        #[test]
        fn leaf_gate_array_element_boundaries() {
            let arr = |items: Vec<ScalarValue>| {
                let mut s = PvStructure::new("epics:nt/NTScalarArray:1.0");
                s.fields.push(("value".into(), PvField::ScalarArray(items)));
                PvField::Structure(s)
            };
            let desc = |st| FieldDesc::Structure {
                struct_id: "epics:nt/NTScalarArray:1.0".into(),
                fields: vec![("value".into(), FieldDesc::ScalarArray(st))],
            };
            // Element type matches descriptor.
            assert!(transformed_leaf_fits_descriptor(
                &arr(vec![ScalarValue::Double(1.0), ScalarValue::Double(2.0)]),
                &desc(ScalarType::Double),
            ));
            // Element type differs from descriptor (ts Array-mode shape).
            assert!(!transformed_leaf_fits_descriptor(
                &arr(vec![ScalarValue::Long(1)]),
                &desc(ScalarType::Double),
            ));
            // Empty array carries no element type -> always fits.
            assert!(transformed_leaf_fits_descriptor(
                &arr(vec![]),
                &desc(ScalarType::Double),
            ));
        }
    }

    /// coalescing two cooked updates under pause/squash.
    /// Tested by the marked-set boundaries, not by narrative: the newer
    /// value always wins; `Some+Some` unions (so a coalesced burst still
    /// marks every field that changed across it, deduped), and any
    /// `None` side collapses to `None` (a server-derived diff over-marks
    /// safely, so it must not be narrowed by a stale explicit set).
    #[test]
    fn br_fr12_coalesce_monitor_update_marked_boundaries() {
        let val = |tag: i32| PvField::Scalar(ScalarValue::Int(tag));
        let upd = |tag: i32, marked: Option<Vec<&str>>| crate::server_native::MonitorUpdate {
            value: val(tag),
            marked: marked.map(|v| v.into_iter().map(str::to_string).collect()),
            type_changed: false,
            overrun: Vec::new(),
        };

        // Some + Some → union of paths, deduped; newer value wins.
        let merged =
            coalesce_monitor_update(upd(1, Some(vec!["a", "b"])), upd(2, Some(vec!["b", "c"])));
        assert_eq!(merged.value, val(2), "newer value must win");
        assert_eq!(
            merged.marked,
            Some(vec!["a".to_string(), "b".to_string(), "c".to_string()]),
            "Some+Some must union marked paths without duplicating `b`"
        );

        // Some(old) + None(new) → None (derive a full/diff bitset).
        let merged = coalesce_monitor_update(upd(1, Some(vec!["a"])), upd(2, None));
        assert_eq!(merged.value, val(2));
        assert!(
            merged.marked.is_none(),
            "a None side must collapse the coalesced set to None"
        );

        // None(old) + Some(new) → None as well.
        let merged = coalesce_monitor_update(upd(1, None), upd(2, Some(vec!["a"])));
        assert_eq!(merged.value, val(2));
        assert!(
            merged.marked.is_none(),
            "a None side must collapse the coalesced set to None (either order)"
        );
    }

    /// Boundary test for the overrun accumulation a squash performs —
    /// pva2pva `moncache.cpp:160-168`:
    ///   overrun |= older.overrun | newer.overrun   (carry forward)
    ///   overrun |= older.changed & newer.changed    (lost intermediate)
    /// A leaf is overrun iff a distinct value for it was overwritten by
    /// the coalesce, i.e. it is marked changed in BOTH the dropped older
    /// update and the surviving newer one. Tested per invariant boundary
    /// (intersection non-empty / disjoint / None side / explicit carry /
    /// dedup), not by a narrative burst.
    #[test]
    fn coalesce_monitor_update_accumulates_overrun() {
        let val = |tag: i32| PvField::Scalar(ScalarValue::Int(tag));
        let s = |v: &[&str]| v.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        let mk = |tag: i32, marked: Option<&[&str]>, overrun: &[&str]| {
            crate::server_native::MonitorUpdate {
                value: val(tag),
                marked: marked.map(s),
                type_changed: false,
                overrun: s(overrun),
            }
        };
        let sorted = |mut v: Vec<String>| {
            v.sort();
            v
        };

        // Intersection non-empty: `b` changed in both → overrun {b}; the
        // value and the changed-union still resolve as before.
        let m =
            coalesce_monitor_update(mk(1, Some(&["a", "b"]), &[]), mk(2, Some(&["b", "c"]), &[]));
        assert_eq!(m.value, val(2), "newer value wins");
        assert_eq!(
            sorted(m.overrun),
            s(&["b"]),
            "only the leaf marked in BOTH updates is overrun"
        );

        // Disjoint marked: no leaf was overwritten → no overrun.
        let m = coalesce_monitor_update(mk(1, Some(&["a"]), &[]), mk(2, Some(&["b"]), &[]));
        assert!(
            m.overrun.is_empty(),
            "disjoint changed sets lose no intermediate — empty overrun"
        );

        // Explicit overrun from either side carries forward (union),
        // even when no fresh intersection arises.
        let m = coalesce_monitor_update(mk(1, Some(&["a"]), &["x"]), mk(2, Some(&["b"]), &["y"]));
        assert_eq!(
            sorted(m.overrun),
            s(&["x", "y"]),
            "producer-recorded overrun on both sides must survive the squash"
        );

        // A None marked side contributes no intersection term, but an
        // explicit overrun it already carried still propagates.
        let m = coalesce_monitor_update(mk(1, None, &["x"]), mk(2, Some(&["b"]), &[]));
        assert_eq!(
            m.overrun,
            s(&["x"]),
            "None side names no leaves, but its recorded overrun carries"
        );
        assert!(m.marked.is_none(), "None side still collapses marked");

        // Dedup: a leaf that is both already-overrun and freshly
        // intersected appears once.
        let m = coalesce_monitor_update(mk(1, Some(&["b"]), &["b"]), mk(2, Some(&["b"]), &[]));
        assert_eq!(
            m.overrun,
            s(&["b"]),
            "overrun must not duplicate a leaf present via both carry and intersection"
        );

        // A type-change boundary discards value/marked/overrun — the
        // squash must surface the boundary, not a stale overrun.
        let m = coalesce_monitor_update(
            mk(1, Some(&["b"]), &["b"]),
            crate::server_native::MonitorUpdate::type_change(),
        );
        assert!(m.type_changed, "boundary survives the squash");
        assert!(m.overrun.is_empty(), "boundary carries no overrun");
    }

    /// R12-32 — the `testmask` gate. pvxs `doPost` decides `real` BEFORE
    /// touching the queue (`servermon.cpp:252-268`): the first post always
    /// goes through, every later one must have a marked leaf inside `pvMask`
    /// (`testmask`, `pvrequest.cpp:73-92`), and a masked-out post is dropped
    /// — it does NOT occupy a FIFO slot and so cannot coalesce a real update
    /// out of the tail.
    ///
    /// Tested by invariant boundary: first-post exemption, marked-inside-mask,
    /// marked-outside-mask, the unmarked (`marked: None`) post that pvxs sees
    /// as fully marked, the terminal boundary, and the squash-contents case
    /// where the drop is what keeps a real update alive.
    #[test]
    fn monitor_queue_drops_updates_outside_the_request_mask() {
        // { value, alarm { severity } } — bits: 0 root, 1 value, 2 alarm,
        // 3 alarm.severity.
        let intro = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
                (
                    "alarm".into(),
                    FieldDesc::Structure {
                        struct_id: "alarm_t".into(),
                        fields: vec![("severity".into(), FieldDesc::Scalar(ScalarType::Int))],
                    },
                ),
            ],
        };
        // The client asked for `field(value)` only.
        let mut mask = BitSet::new();
        mask.set(1);

        let upd = |tag: i32, marked: &[&str]| crate::server_native::MonitorUpdate {
            value: PvField::Scalar(ScalarValue::Int(tag)),
            marked: Some(marked.iter().map(|s| s.to_string()).collect()),
            type_changed: false,
            overrun: Vec::new(),
        };
        let tags = |q: &MonitorQueue| {
            q.pending
                .iter()
                .map(|u| u.value.clone())
                .collect::<Vec<PvField>>()
        };
        let tag = |t: i32| PvField::Scalar(ScalarValue::Int(t));

        // No seed: `first` is still set, so the FIRST post is exempt from the
        // mask test even though `alarm.severity` lies outside `field(value)`.
        let mut q = MonitorQueue::new(4, &intro, &mask);
        assert!(
            q.push(upd(1, &["alarm.severity"])),
            "the first post is always queued (MonitorOp::first)"
        );
        // ...and every later masked-out post is dropped.
        assert!(
            !q.push(upd(2, &["alarm.severity"])),
            "a post whose marked leaves lie outside pvMask must be dropped"
        );
        assert!(
            q.push(upd(3, &["value"])),
            "a post marking a selected leaf is queued"
        );
        assert_eq!(
            tags(&q),
            vec![tag(1), tag(3)],
            "the masked-out post never entered the FIFO"
        );

        // A seeded op consumes the `first` exemption, so its very first stream
        // event is already mask-tested.
        let mut q = MonitorQueue::new(4, &intro, &mask);
        q.seed(tag(0).into());
        assert!(
            !q.push(upd(1, &["alarm.severity"])),
            "the seed IS pvxs's first post; the next event is mask-tested"
        );
        assert_eq!(tags(&q), vec![tag(0)], "only the seed is queued");

        // A source that marks nothing posts a wholly-changed value — pvxs's
        // fully-marked Value — so it passes `testmask` for any mask that
        // selects at least one LEAF. `field(value)` does.
        let mut q = MonitorQueue::new(4, &intro, &mask);
        q.seed(tag(0).into());
        assert!(
            q.push(crate::server_native::MonitorUpdate {
                value: tag(1),
                marked: None,
                type_changed: false,
                overrun: Vec::new(),
            }),
            "an unmarked post is fully marked to pvxs; the mask selects a leaf"
        );

        // The terminal boundary is pvxs's null Value (`if(real || !val)`): it
        // must queue regardless of the mask, or the MONITOR FINISH is lost.
        let mut q = MonitorQueue::new(4, &intro, &mask);
        q.seed(tag(0).into());
        assert!(
            q.push(crate::server_native::MonitorUpdate::type_change()),
            "a descriptor boundary always queues"
        );

        // R13-34: the terminal on a FULL FIFO. pvxs's append gate is
        // `(mon->queue.size() < mon->limit) || force || !val`
        // (servermon.cpp:270-283) — a terminal is ALWAYS push_back'd and
        // grows the queue PAST `limit`. It must never reach the squash
        // branch, which would pop the newest real update and coalesce the
        // terminal over it (`coalesce_monitor_update` returns a bare
        // `type_change()` whenever either side is `type_changed`).
        //
        // pvxs delivers all `limit` queued updates and THEN the FINISH;
        // pre-fix the port delivered `limit - 1` and the FINISH.
        let mut q = MonitorQueue::new(2, &intro, &mask);
        q.seed(tag(0).into());
        assert!(q.push(upd(1, &["value"])), "fills the FIFO to limit=2");
        assert_eq!(tags(&q), vec![tag(0), tag(1)], "FIFO is full");
        assert!(
            q.push(crate::server_native::MonitorUpdate::type_change()),
            "a terminal always queues, even on a full FIFO"
        );
        assert_eq!(
            q.pending.len(),
            3,
            "the terminal grows the queue past limit=2 (pvxs push_back)"
        );
        assert_eq!(
            tags(&q),
            vec![tag(0), tag(1), PvField::Null],
            "both real updates survive and the terminal trails them — it must \
             not squash the newest one out of the tail"
        );
        assert!(
            q.pending[2].type_changed,
            "the terminal is the last entry, delivered after every real update"
        );

        // Squash CONTENTS: with limit 2 and the FIFO already full, a
        // masked-out post must not coalesce the real tail. Pre-fix it took a
        // slot and overwrote `value=3` with `value=4`.
        let mut q = MonitorQueue::new(2, &intro, &mask);
        q.seed(tag(0).into());
        q.push(upd(3, &["value"]));
        assert!(!q.push(upd(4, &["alarm.severity"])), "masked out → dropped");
        assert_eq!(
            tags(&q),
            vec![tag(0), tag(3)],
            "a masked-out post must not squash a real update out of the tail"
        );
    }

    /// R14-31 — the enqueue gate and the wire changed-bitset must be ONE
    /// computation. pvxs's `testmask` (`pvrequest.cpp:73-92`) scans
    /// `store[idx].valid && mask[idx]`, and only leaves ever carry `valid`,
    /// so its gate ranges over exactly the bits `to_wire_valid` emits. A
    /// mask can hold a STRUCTURE bit with no leaf under it — `field(
    /// timeStamp.bogus)`, a client typo: `request2mask` marks the matched
    /// `timeStamp` structure and the non-existent child selects nothing.
    /// Gating on the raw marked bitset (which carries structure bits) admits
    /// such a post and then frames an EMPTY changed-bitset at full event
    /// rate, where pvxs sends nothing at all.
    ///
    /// R15-32 extends this to the `marked: None` arm, which used to be waved
    /// through unconditionally: `testmask` is a leaf test whatever the source
    /// marked, so a leafless mask silences the subscription for a fully-marked
    /// `Value` too.
    ///
    /// Tested per boundary, on BOTH arms (declared leaf set / no leaf set):
    /// mask with a structure bit but no leaf (drop, and the frame that was
    /// avoided is provably empty), and a mask that does select a leaf below it
    /// (admit, and the frame carries exactly that leaf).
    #[test]
    fn monitor_queue_drops_a_post_whose_wire_bitset_would_be_empty() {
        // { value, timeStamp { secondsPastEpoch, nanoseconds } } — bits:
        // 0 root, 1 value, 2 timeStamp, 3 secondsPastEpoch, 4 nanoseconds.
        let intro = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
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
        let empty_struct = || FieldDesc::Structure {
            struct_id: String::new(),
            fields: Vec::new(),
        };
        // pvRequest `field(timeStamp.<leaf>)`, built as the wire type is.
        let request = |leaf: &str| FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![(
                "field".into(),
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![(
                        "timeStamp".into(),
                        FieldDesc::Structure {
                            struct_id: String::new(),
                            fields: vec![(leaf.into(), empty_struct())],
                        },
                    )],
                },
            )],
        };
        let post = || crate::server_native::MonitorUpdate {
            value: PvField::Scalar(ScalarValue::Int(1)),
            marked: Some(vec!["timeStamp".to_string()]),
            type_changed: false,
            overrun: Vec::new(),
        };
        let seed = PvField::Scalar(ScalarValue::Int(0));

        // The typo: `field(timeStamp.bogus)` selects the `timeStamp`
        // STRUCTURE bit and no leaf.
        let bogus = crate::pv_request::request_to_mask(&intro, Some(&request("bogus")))
            .expect("a matched structure keeps request2mask's foundrequested");
        assert!(bogus.get(2), "the matched timeStamp structure bit is set");
        assert!(
            !bogus.get(3) && !bogus.get(4),
            "no leaf below it is selected"
        );

        let mut q = MonitorQueue::new(4, &intro, &bogus);
        q.seed(seed.clone().into());
        assert!(
            !q.push(post()),
            "a post marking only a structure bit inside pvMask carries no wire \
             leaf — pvxs's testmask drops it"
        );
        assert!(
            crate::pvdata::encode::marked_wire_changed_bitset(
                &intro,
                &["timeStamp".to_string()],
                &bogus,
            )
            .is_empty(),
            "the frame the gate refused would have had an empty changed-bitset"
        );

        // Same marked path, but the request names a real leaf: admitted, and
        // the wire bitset is exactly that leaf.
        let real = crate::pv_request::request_to_mask(&intro, Some(&request("secondsPastEpoch")))
            .expect("a matched leaf selects it");
        let mut q = MonitorQueue::new(4, &intro, &real);
        q.seed(seed.clone().into());
        assert!(
            q.push(post()),
            "a marked subtree with a selected leaf posts"
        );
        let changed = crate::pvdata::encode::marked_wire_changed_bitset(
            &intro,
            &["timeStamp".to_string()],
            &real,
        );
        assert_eq!(
            changed.iter().collect::<Vec<usize>>(),
            vec![3],
            "the frame carries the one selected leaf, not the structure bit"
        );

        // R15-32 — the SAME boundary on the `marked: None` arm. pvxs's
        // `testmask` is a leaf test whatever the source marked, so a leafless
        // mask silences the subscription for a fully-marked Value too. The
        // gate admitted every unmarked post, so `field(timeStamp.bogus)` drew
        // full-rate DATA frames with an empty changed-bitset where pvxs sends
        // nothing.
        let unmarked = || crate::server_native::MonitorUpdate {
            value: PvField::Scalar(ScalarValue::Int(1)),
            marked: None,
            type_changed: false,
            overrun: Vec::new(),
        };
        let mut q = MonitorQueue::new(4, &intro, &bogus);
        q.seed(seed.clone().into());
        assert!(
            !q.push(unmarked()),
            "an unmarked post under a leafless mask frames no leaf — dropped"
        );
        assert!(
            crate::pvdata::encode::canonical_changed_bitset(&intro, &bogus).is_empty(),
            "the frame the gate refused would have had an empty changed-bitset"
        );

        // A mask that selects a leaf admits the same unmarked post, and the
        // frame carries that leaf — gate and wire stay one computation.
        let mut q = MonitorQueue::new(4, &intro, &real);
        q.seed(seed.into());
        assert!(
            q.push(unmarked()),
            "an unmarked post under a leafful mask is real"
        );
        assert_eq!(
            crate::pvdata::encode::canonical_changed_bitset(&intro, &real)
                .iter()
                .collect::<Vec<usize>>(),
            vec![3],
            "the admitted frame carries exactly the selected leaf"
        );
    }

    /// Boundary test for the bounded server-side monitor FIFO
    /// ([`push_squash_monitor`], pvxs servermon.cpp:271-287). The producer
    /// appends each post as a DISTINCT entry while the FIFO holds fewer than
    /// `limit`; once full, the newest post coalesces into the tail (unioning
    /// marked-leaf sets via [`coalesce_monitor_update`]). This single
    /// rule governs both the Idle-accrual windows (INIT->START, STOP->START)
    /// and the Executing burst, so a `record[queueSize=N]` monitor never holds
    /// more than `N` unsent posts. Tested by invariant boundary — below limit,
    /// at limit, the `limit == 1` collapse, and a sticky descriptor boundary —
    /// not by a narrative burst.
    #[test]
    fn push_squash_monitor_bounds_fifo_to_queue_size() {
        use std::collections::VecDeque;
        let val = |tag: i32| PvField::Scalar(ScalarValue::Int(tag));
        let upd = |tag: i32| crate::server_native::MonitorUpdate {
            value: val(tag),
            marked: None,
            type_changed: false,
            overrun: Vec::new(),
        };
        let values = |pending: &VecDeque<crate::server_native::MonitorUpdate>| {
            pending.iter().map(|u| u.value.clone()).collect::<Vec<_>>()
        };

        // Below the limit: three posts into limit 4 stay distinct, in order.
        let mut pending: VecDeque<crate::server_native::MonitorUpdate> = VecDeque::new();
        let mut any_overflow = false;
        for tag in [1, 2, 3] {
            any_overflow |= push_squash_monitor(&mut pending, upd(tag), 4, coalesce_monitor_update);
        }
        assert!(!any_overflow, "three posts below limit 4 must not squash");
        assert_eq!(
            values(&pending),
            vec![val(1), val(2), val(3)],
            "posts stay distinct and ordered below the limit"
        );

        // At the limit: three posts into limit 2 leave [1, 3] — the newest
        // coalesced into the tail, never queue_limit + 1 distinct posts.
        let mut pending: VecDeque<crate::server_native::MonitorUpdate> = VecDeque::new();
        let mut overflow = false;
        for tag in [1, 2, 3] {
            overflow |= push_squash_monitor(&mut pending, upd(tag), 2, coalesce_monitor_update);
        }
        assert!(overflow, "the 3rd post into limit 2 must squash the tail");
        assert_eq!(
            values(&pending),
            vec![val(1), val(3)],
            "queueSize=2 holds the head plus one tail, squashing the newest into it"
        );

        // limit == 1: every later post collapses into the single entry
        // (latest value wins).
        let mut pending: VecDeque<crate::server_native::MonitorUpdate> = VecDeque::new();
        for tag in [1, 2, 3] {
            push_squash_monitor(&mut pending, upd(tag), 1, coalesce_monitor_update);
        }
        assert_eq!(
            values(&pending),
            vec![val(3)],
            "limit 1 collapses the burst to the single newest value"
        );

        // R13-34: a descriptor-change boundary (pvxs's terminal, `!val`)
        // never squashes. pvxs's append gate is
        // `(mon->queue.size() < mon->limit) || force || !val`
        // (servermon.cpp:270-283), so the terminal is push_back'd PAST the
        // limit. Pushed into a FULL limit-1 FIFO it must NOT pop the real
        // update and coalesce over it — the real value survives and the
        // terminal trails it, exactly as pvxs delivers every queued update
        // and then the FINISH.
        let mut pending: VecDeque<crate::server_native::MonitorUpdate> = VecDeque::new();
        push_squash_monitor(&mut pending, upd(2), 1, coalesce_monitor_update);
        let squashed = push_squash_monitor(
            &mut pending,
            crate::server_native::MonitorUpdate::type_change(),
            1,
            coalesce_monitor_update,
        );
        assert!(!squashed, "a terminal never reaches the squash branch");
        assert_eq!(
            pending.len(),
            2,
            "the terminal grows the queue past limit=1 (pvxs push_back)"
        );
        assert_eq!(
            pending[0].value,
            val(2),
            "the newest real update survives the terminal"
        );
        assert!(pending[1].type_changed, "the terminal trails it");

        // Same rule on the RAW-forward FIFO: `RawMonitorEvent` carries the
        // same `type_changed` boundary and coalesces with `|_old, new| new`,
        // so pre-fix a terminal on a full raw queue replaced the newest raw
        // frame outright.
        let raw = |body: &[u8], terminal: bool| crate::server_native::RawMonitorEvent {
            body_bytes: bytes::Bytes::copy_from_slice(body),
            byte_order: ByteOrder::Little,
            type_changed: terminal,
        };
        let mut raw_pending: VecDeque<crate::server_native::RawMonitorEvent> = VecDeque::new();
        push_squash_monitor(&mut raw_pending, raw(b"a", false), 1, |_old, new| new);
        push_squash_monitor(&mut raw_pending, raw(b"", true), 1, |_old, new| new);
        assert_eq!(
            raw_pending.len(),
            2,
            "the raw terminal is push_back'd past limit=1 too"
        );
        assert_eq!(
            raw_pending[0].body_bytes.as_ref(),
            b"a",
            "the newest raw frame is not destroyed by the terminal"
        );
        assert!(raw_pending[1].type_changed, "the raw terminal trails it");
    }

    /// server pipeline parser accepts the typed-bool /
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

    /// Negotiate `req` against the pvxs per-op default limit
    /// (`MonitorOp::limit = 4u`), which is also this server's default
    /// `monitor_queue_depth`.
    fn negotiate_opts(req: &PvField) -> Result<MonitorPipelineRequest, NoConvert> {
        monitor_pipeline_options(
            req,
            crate::server_native::source::DEFAULT_MONITOR_QUEUE_LIMIT,
        )
    }

    /// Unwrap the parsed options, asserting the request was NOT a
    /// pipeline-negotiation reject and did NOT throw.
    fn parsed_opts(req: &PvField) -> PipelineOptions {
        match negotiate_opts(req) {
            Ok(MonitorPipelineRequest::Options(o)) => o,
            Ok(MonitorPipelineRequest::Reject(msg)) => {
                panic!("expected parsed options, got a pipeline-negotiation Reject: {msg}")
            }
            Err(e) => panic!("expected parsed options, got a circuit-resetting NoConvert: {e}"),
        }
    }

    #[test]
    fn pva_r20_pipeline_typed_bool_true_enables_window() {
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(16)),
        );
        let opts = parsed_opts(&req);
        assert!(opts.enabled, "Boolean(true) must enable pipeline");
        assert_eq!(opts.queue_size, 16);
    }

    #[test]
    fn pva_r20_pipeline_string_true_still_enables_window() {
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::String("true".into())),
            PvField::Scalar(ScalarValue::String("32".into())),
        );
        let opts = parsed_opts(&req);
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
        let opts = parsed_opts(&req);
        assert!(opts.enabled, "Int(1) must enable pipeline");
        assert_eq!(opts.queue_size, 8);
    }

    #[test]
    fn pva_r20_pipeline_queue_size_below_two_rejects() {
        // pvxs `servermon.cpp:537-540`: pipeline=true with a PRESENT
        // queueSize < 2 is a negotiation error (`ctrl->error(...)` +
        // `return`), not a silent downgrade to a non-pipeline monitor.
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(1)),
        );
        assert!(
            matches!(negotiate_opts(&req), Ok(MonitorPipelineRequest::Reject(_))),
            "pipeline + queueSize<2 must reject the INIT, not downgrade",
        );
    }

    #[test]
    fn pva_r20_pipeline_unparseable_queue_size_rejects() {
        // PRESENT but unconvertible queueSize under pipeline → Reject
        // (pvxs `queueSize.as(qSize)` fails, then `op->pipeline` → error).
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::String("not-a-number".into())),
        );
        let got = negotiate_opts(&req);
        let Ok(MonitorPipelineRequest::Reject(msg)) = got else {
            panic!("pipeline + unconvertible queueSize must reject the INIT");
        };
        // pvxs `ctrl->error(SB()<<"can not pipeline invalid queueSize : "
        // <<queueSize)` — the offending value, no invented "(must be >= 2)".
        assert!(
            msg.starts_with("can not pipeline invalid queueSize : "),
            "pvxs error text: {msg}"
        );
        assert!(msg.contains("not-a-number"), "the value is named: {msg}");
    }

    /// R10-36: `SB()<<queueSize` streams the option `Value` through pvxs's
    /// DEFAULT (tree) formatter — `<typecode> = <value>` plus the newline the
    /// formatter always writes — not the bare scalar text this used to append.
    /// Every diagnostic that names an option value shares that one renderer
    /// ([`crate::pvdata::render_value`]) with the QSRV bridge.
    #[test]
    fn pva_r10_36_option_diagnostics_render_the_value_like_pvxs() {
        // Reject text (`servermon.cpp:538`).
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::String("not-a-number".into())),
        );
        let Ok(MonitorPipelineRequest::Reject(msg)) = negotiate_opts(&req) else {
            panic!("pipeline + unconvertible queueSize must reject the INIT");
        };
        assert_eq!(
            msg, "can not pipeline invalid queueSize : string = \"not-a-number\"\n",
            "pre-R10-36 this rendered the bare scalar (`not-a-number`)"
        );

        // Warn text on a NON-pipeline monitor (`servermon.cpp:542`).
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(false)),
            PvField::Scalar(ScalarValue::String("garbage".into())),
        );
        let opts = parsed_opts(&req);
        assert_eq!(
            opts.diagnostics[0].message,
            "Unable to use record._options.queueSize : string = \"garbage\"\n"
        );

        // `pipeline` Warn (`servermon.cpp:529`) — a string `as<bool>` refuses.
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::String("maybe".into())),
            PvField::Scalar(ScalarValue::Int(8)),
        );
        let opts = parsed_opts(&req);
        assert_eq!(
            opts.diagnostics[0].message,
            "Unable to parse record._options.pipeline : string = \"maybe\"\n"
        );

        // Negative control: a NON-scalar option is no longer flattened to
        // `<non-scalar>`; it renders as pvxs's tree form for that value.
        let req = make_pipeline_request(
            PvField::ScalarArrayTyped(crate::pvdata::TypedScalarArray::Int(vec![1, 2].into())),
            PvField::Scalar(ScalarValue::Int(8)),
        );
        let opts = parsed_opts(&req);
        assert_eq!(
            opts.diagnostics[0].message,
            "Unable to parse record._options.pipeline : int32_t[] = {2}[1, 2]\n"
        );
    }

    /// R10-32: pvxs `queueSize.as(qSize)` converts a REAL
    /// (`uint64_t(double(src))`) and parses a STRING with `parseTo<uint64_t>`
    /// = `stoull(s,&idx,0)`, base 0. The port dropped Float/Double and read
    /// strings as decimal-only, so each of these was refused: under pipeline
    /// with the port's invented "can not pipeline invalid queueSize" error,
    /// and on a plain monitor with a spurious Warn plus the default depth.
    #[test]
    fn pva_r10_32_queue_size_converts_reals_and_base_zero_strings() {
        for (v, want) in [
            (ScalarValue::Double(8.0), 8u32),
            (ScalarValue::Double(8.9), 8),
            (ScalarValue::Float(16.0), 16),
            (ScalarValue::String("0x10".into()), 16),
            (ScalarValue::String("010".into()), 8),
            (ScalarValue::String("12".into()), 12),
            (ScalarValue::UInt(5), 5),
        ] {
            let req = make_pipeline_request(
                PvField::Scalar(ScalarValue::Boolean(true)),
                PvField::Scalar(v.clone()),
            );
            let opts = parsed_opts(&req);
            assert!(opts.enabled, "queueSize = {v:?} must not disable pipeline");
            assert_eq!(opts.queue_size, want, "queueSize = {v:?}");
            assert!(
                opts.diagnostics.is_empty(),
                "a converted queueSize must not warn: {:?}",
                opts.diagnostics
            );
        }
    }

    /// R10-32 (non-pipeline half): the same conversion feeds `op->limit` for a
    /// PLAIN monitor — no pipeline, no Warn, requested depth honored.
    #[test]
    fn pva_r10_32_real_queue_size_sets_a_plain_monitor_depth() {
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(false)),
            PvField::Scalar(ScalarValue::Double(8.0)),
        );
        let opts = parsed_opts(&req);
        assert!(!opts.enabled);
        // `op->limit = qSize` sits OUTSIDE `if(op->pipeline)`, so the
        // squash override applies to a non-pipeline monitor too.
        assert_eq!(opts.queue_size, 8);
        assert!(
            opts.diagnostics.is_empty(),
            "a converted queueSize must not warn: {:?}",
            opts.diagnostics
        );
    }

    /// Build a pvRequest carrying exactly the named `record._options`.
    fn make_options_request(pairs: &[(&str, PvField)]) -> PvField {
        let options = PvField::Structure(PvStructure {
            struct_id: String::new(),
            fields: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
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

    /// R11-31 — the per-op queue limit is ONE value: pvxs seeds
    /// `uint32_t qSize = op->limit` from the `MonitorOp::limit = 4u`
    /// initializer (servermon.cpp:66) and overwrites it only for a valid
    /// `queueSize >= 2` (`:533-543`). That single `op->limit` is the squash
    /// threshold (`:273`), the base of the `ackAny` arithmetic
    /// (`:564,578,581`) and the reported depth (`:313`).
    ///
    /// The port kept TWO: the squash depth defaulted to the server-wide
    /// `monitor_queue_depth` (64) while the SAME negotiation defaulted the
    /// `ackAny` base to a hardcoded 4. The assertions below distinguish them —
    /// with a non-default server default, the pre-fix code answered 4 where
    /// pvxs answers the server's limit.
    #[test]
    fn pva_r11_31_one_negotiated_limit_seeded_from_the_server_default() {
        use crate::server_native::runtime::PvaServerConfig;

        // The default IS the pvxs per-op initializer, not a 64-deep queue.
        assert_eq!(
            PvaServerConfig::default().monitor_queue_depth,
            4,
            "pvxs MonitorOp::limit = 4u (servermon.cpp:66)"
        );
        assert_eq!(
            PvaServerConfig::default().monitor_queue_limit(),
            crate::server_native::source::DEFAULT_MONITOR_QUEUE_LIMIT
        );
        // The documented `depth * 3 / 4` relation held for 64/48 and must
        // still hold now that the depth is the pvxs per-op default.
        assert_eq!(
            PvaServerConfig::default().monitor_high_watermark,
            PvaServerConfig::default().monitor_queue_depth * 3 / 4
        );

        // A deployment that raised the per-op default — the same deviation as
        // building pvxs with a different `limit` initializer.
        const SERVER_DEFAULT: u32 = 16;

        // No `record._options` at all: nothing negotiated, so the limit is the
        // server's. (The parser used to answer `None` here and leave each
        // consumer to pick its own fallback.)
        let plain = PvField::Structure(PvStructure {
            struct_id: String::new(),
            fields: vec![],
        });
        let Ok(MonitorPipelineRequest::Options(o)) =
            monitor_pipeline_options(&plain, SERVER_DEFAULT)
        else {
            panic!("a pvRequest with no _options negotiates the defaults");
        };
        assert!(!o.enabled);
        assert_eq!(o.queue_size, SERVER_DEFAULT);

        // Pipeline ON, queueSize ABSENT, `ackAny = 50%`: pvxs takes the percent
        // of `op->limit`, which is the SERVER default here — 8, not 2 (50% of
        // the 4 the port hardcoded as the pipeline default).
        let req = make_options_request(&[
            ("pipeline", PvField::Scalar(ScalarValue::Boolean(true))),
            ("ackAny", PvField::Scalar(ScalarValue::String("50%".into()))),
        ]);
        let Ok(MonitorPipelineRequest::Options(o)) = monitor_pipeline_options(&req, SERVER_DEFAULT)
        else {
            panic!("an absent queueSize is not a negotiation error");
        };
        assert!(o.enabled);
        assert_eq!(
            o.queue_size, SERVER_DEFAULT,
            "absent queueSize keeps op->limit"
        );
        assert_eq!(
            o.ack_at, 8,
            "ackAny percent is a fraction of the NEGOTIATED limit"
        );

        // Pipeline ON, queueSize ABSENT, ackAny PRESENT but zero: 0 is below the
        // representable minimum, so the [1, limit] clamp takes it to 1.
        // CBUG-B12: pvxs runs `if(ackAt==0) ackAt = op->limit/2`
        // (servermon.cpp:578) — this assertion used to read SERVER_DEFAULT / 2.
        // What this case still pins for R11-31 is that the ack base is the
        // NEGOTIATED limit, which the percentage case above covers.
        let req = make_options_request(&[
            ("pipeline", PvField::Scalar(ScalarValue::Boolean(true))),
            ("ackAny", PvField::Scalar(ScalarValue::UInt(0))),
        ]);
        let Ok(MonitorPipelineRequest::Options(o)) = monitor_pipeline_options(&req, SERVER_DEFAULT)
        else {
            panic!("pipeline with a zero ackAny is a valid request");
        };
        assert_eq!(o.queue_size, SERVER_DEFAULT);
        assert_eq!(o.ack_at, 1);

        // A valid client `queueSize` still wins over the server default, and it
        // is the ack base too (`ackAny=50%` of 8 → 4).
        let req = make_options_request(&[
            ("pipeline", PvField::Scalar(ScalarValue::Boolean(true))),
            ("queueSize", PvField::Scalar(ScalarValue::UInt(8))),
            ("ackAny", PvField::Scalar(ScalarValue::String("50%".into()))),
        ]);
        let Ok(MonitorPipelineRequest::Options(o)) = monitor_pipeline_options(&req, SERVER_DEFAULT)
        else {
            panic!("a valid queueSize is not a negotiation error");
        };
        assert_eq!(o.queue_size, 8);
        assert_eq!(o.ack_at, 4);

        // Boundary (negative control): `queueSize = 1` is REFUSED (pvxs
        // `qSize>=2`) on a plain monitor — the limit stays the server default
        // and a Warn is logged, even when the server default is itself 1, so a
        // refused request is never mistaken for an accepted one.
        let req = make_options_request(&[("queueSize", PvField::Scalar(ScalarValue::UInt(1)))]);
        let Ok(MonitorPipelineRequest::Options(o)) = monitor_pipeline_options(&req, 1) else {
            panic!("an invalid queueSize on a plain monitor is a Warn, not a reject");
        };
        assert_eq!(o.queue_size, 1, "the server default stands");
        assert_eq!(
            o.diagnostics.len(),
            1,
            "a refused queueSize warns even when it equals the default"
        );
    }

    /// R10-32 (boundary): a negative / oversized integer WRAPS through
    /// `uint64_t(int64_t(src))` and the `uint32_t` narrowing rather than being
    /// refused — pvxs sets `op->limit = 0xFFFF_FFFF`. NOT a rejection.
    #[test]
    fn pva_r10_32_negative_queue_size_wraps_like_the_c_cast() {
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(-1)),
        );
        let opts = parsed_opts(&req);
        assert!(opts.enabled);
        assert_eq!(opts.queue_size, 0xFFFF_FFFF);
    }

    /// R10-32 (documented non-divergence): a BOOLEAN queueSize converts to
    /// 0/1, which is `< 2`, so both pvxs and the port treat it as invalid.
    #[test]
    fn pva_r10_32_boolean_queue_size_is_still_invalid() {
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Boolean(true)),
        );
        assert!(
            matches!(negotiate_opts(&req), Ok(MonitorPipelineRequest::Reject(_))),
            "Boolean queueSize converts to 1, which is < 2 → invalid",
        );
    }

    /// R10-31: pvxs `pipeline.as(v)` routes a REAL through `copyOutScalar`
    /// → `bool(src)` (`servermon.cpp:525`, `data.cpp:405`), so
    /// `pipeline = Double(1.0)` enables the credit-windowed pipeline
    /// sub-protocol. The port hardcoded Float/Double to `false`, serving a
    /// plain monitor where pvxs serves a pipelined one — a different wire
    /// flow-control shape, with any accompanying `ackAny` silently dropped.
    #[test]
    fn pva_r10_31_real_pipeline_converts_by_nonzero() {
        for (v, want) in [
            (ScalarValue::Double(1.0), true),
            (ScalarValue::Double(0.5), true),
            (ScalarValue::Double(0.0), false),
            (ScalarValue::Float(1.0), true),
            (ScalarValue::Float(0.0), false),
        ] {
            let req = make_pipeline_request(
                PvField::Scalar(v.clone()),
                PvField::Scalar(ScalarValue::Int(8)),
            );
            let opts = parsed_opts(&req);
            assert_eq!(opts.enabled, want, "pipeline = {v:?} must convert as bool");
            // A converted option draws NO diagnostic — pvxs `as(v)` succeeded.
            assert!(
                opts.diagnostics.is_empty(),
                "a converted pipeline value must not warn: {:?}",
                opts.diagnostics
            );
        }
    }

    /// R10-31 (same owner): pvxs `Value::as<bool>` accepts ONLY the exact
    /// string tokens `"true"`/`"false"` (`data.cpp:466-469`). The port's
    /// hand-rolled match also took `"1"`/`"yes"`/`"0"`/`"no"` and case-folded,
    /// so `pipeline="1"` enabled a pipeline pvxs leaves DISABLED (with a Warn).
    #[test]
    fn pva_r10_31_unconvertible_pipeline_string_warns_and_disables() {
        for s in ["1", "yes", "0", "no", "TRUE", "True"] {
            let req = make_pipeline_request(
                PvField::Scalar(ScalarValue::String(s.into())),
                PvField::Scalar(ScalarValue::Int(8)),
            );
            let opts = parsed_opts(&req);
            assert!(
                !opts.enabled,
                "pvxs as<bool> refuses {s:?}: pipeline stays disabled"
            );
            assert_eq!(
                opts.diagnostics.len(),
                1,
                "an unconvertible pipeline value draws the servermon.cpp:529 Warn"
            );
            assert_eq!(opts.diagnostics[0].level, MessageType::Warning);
        }
    }

    #[test]
    fn pva_r20_pipeline_absent_queue_size_keeps_default_window() {
        // pvxs keeps the default `limit` (4) when queueSize is ABSENT;
        // pipeline stays enabled. Only a PRESENT-invalid queueSize
        // errors — an absent one is not a negotiation failure.
        let options = PvField::Structure(PvStructure {
            struct_id: String::new(),
            fields: vec![(
                "pipeline".to_string(),
                PvField::Scalar(ScalarValue::Boolean(true)),
            )],
        });
        let record = PvField::Structure(PvStructure {
            struct_id: String::new(),
            fields: vec![("_options".to_string(), options)],
        });
        let req = PvField::Structure(PvStructure {
            struct_id: String::new(),
            fields: vec![("record".to_string(), record)],
        });
        let opts = parsed_opts(&req);
        assert!(opts.enabled, "absent queueSize must NOT disable pipeline");
        assert_eq!(opts.queue_size, 4, "absent queueSize → default depth 4");
    }

    #[test]
    fn pva_r20_non_pipeline_invalid_queue_size_does_not_reject() {
        // pipeline=false + invalid queueSize: pvxs warns and ignores
        // (keeps default limit), it does NOT error. So no Reject, and
        // the queue depth falls back to the default 4.
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(false)),
            PvField::Scalar(ScalarValue::Int(1)),
        );
        let opts = parsed_opts(&req);
        assert!(!opts.enabled, "pipeline=false stays non-pipeline");
        assert_eq!(opts.queue_size, 4, "invalid queueSize → default 4");
    }

    /// Finding (Medium): the INIT pvRequest VALUE decode must
    /// distinguish an ABSENT body (cursor exhausted after the
    /// descriptor — tolerated for the Rust client's RPC INIT) from a
    /// PRESENT but malformed one (an INIT protocol error). The previous
    /// `decode_pv_field(..).ok()` collapsed both into `None`, silently
    /// dropping `_filter` / pipeline / `process`|`block` options.
    /// Tested by the presence/validity boundary, not by scenario.
    mod decode_init_pv_request_value_owner {
        use super::*;

        /// A descriptor that requires value bytes (scalar Int) but whose
        /// frame ends before them is the `from_wire_full` -> `!M.good()`
        /// wire fault pvxs `bev.reset()`s on (`dataencode.cpp:747-752`):
        /// connection-fatal, not a silently-tolerated `None`.
        #[test]
        fn absent_body_for_scalar_descriptor_is_err() {
            let desc = FieldDesc::Scalar(ScalarType::Int);
            let buf: &[u8] = &[];
            let mut cur = std::io::Cursor::new(buf);
            assert!(
                decode_init_pv_request_value(
                    &mut cur,
                    &desc,
                    ByteOrder::Little,
                    &mut TypeCache::new()
                )
                .is_err(),
                "a non-null descriptor needing value bytes with none present \
                 must fault, not collapse to None",
            );
        }

        /// The default `field(...)` selector: a descriptor whose
        /// sub-structures are all empty consumes zero value bytes and
        /// `from_wire_full` stays good. With the buffer exhausted this
        /// carries no create-time options, so it stays `None` — the
        /// content-less contract the option consumers rely on.
        #[test]
        fn absent_body_for_empty_struct_descriptor_is_none() {
            // `structure { structure field { structure value {} } }` —
            // the shape `pv_request::build(&["value"])` emits.
            let desc = FieldDesc::Structure {
                struct_id: String::new(),
                fields: vec![(
                    "field".to_string(),
                    FieldDesc::Structure {
                        struct_id: String::new(),
                        fields: vec![(
                            "value".to_string(),
                            FieldDesc::Structure {
                                struct_id: String::new(),
                                fields: vec![],
                            },
                        )],
                    },
                )],
            };
            let buf: &[u8] = &[];
            let mut cur = std::io::Cursor::new(buf);
            assert!(matches!(
                decode_init_pv_request_value(
                    &mut cur,
                    &desc,
                    ByteOrder::Little,
                    &mut TypeCache::new()
                ),
                Ok(None)
            ));
        }

        /// Present, well-formed value body: decoded to `Some`.
        #[test]
        fn present_valid_body_decodes() {
            let desc = FieldDesc::Scalar(ScalarType::Int);
            let buf: &[u8] = &[42, 0, 0, 0]; // i32 LE = 42
            let mut cur = std::io::Cursor::new(buf);
            assert_eq!(
                decode_init_pv_request_value(
                    &mut cur,
                    &desc,
                    ByteOrder::Little,
                    &mut TypeCache::new()
                ),
                Ok(Some(PvField::Scalar(ScalarValue::Int(42)))),
            );
        }

        /// Present but malformed body (descriptor needs 4 bytes for an
        /// Int, only 2 are there): the formerly-swallowed path — now an
        /// error rather than a silent `None`.
        #[test]
        fn present_malformed_body_is_err() {
            let desc = FieldDesc::Scalar(ScalarType::Int);
            let buf: &[u8] = &[1, 2]; // truncated i32
            let mut cur = std::io::Cursor::new(buf);
            assert!(
                decode_init_pv_request_value(
                    &mut cur,
                    &desc,
                    ByteOrder::Little,
                    &mut TypeCache::new()
                )
                .is_err(),
                "a present-but-truncated value body must error, not collapse to None",
            );
        }
    }

    /// Regression (PVA parity): a peer wire-decode fault of the INIT
    /// pvRequest on the shared GET/PUT/RPC/MONITOR handler is
    /// connection-fatal, matching pvxs `from_wire_type_value` +
    /// `if(!M.good()) bev.reset()` (`serverget.cpp:371-375`,
    /// `servermon.cpp:489-502`). The pre-fix handler replied with a
    /// per-op Status and returned `Ok(())`, leaving a malformed-INIT peer
    /// free to keep reusing the connection. Verified for both GET and
    /// MONITOR via a present-but-truncated pvRequest value.
    #[epics_macros_rs::epics_test]
    async fn init_malformed_pvrequest_value_is_connection_fatal() {
        use crate::server_native::SharedSource;
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::shared_pv::SharedPV;

        async fn run(kind: OpKind, ioid: u32) -> PvaResult<()> {
            let order = ByteOrder::Little;
            let sid: u32 = 1;
            let intro = three_field_intro();
            let pv = SharedPV::new();
            pv.open(intro.clone(), three_field_value(0, 0, 0)).unwrap();
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
                    introspection: Some(std::sync::Arc::new(intro)),
                    source,
                    stat: crate::server_native::peers::ChannelStat::new(String::new()),
                    open_cred: ClientCredentials::anonymous(TEST_PEER),
                    ops: HashMap::new(),
                },
            );
            let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
            let config = PvaServerConfig::default();
            let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
            let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
            let cred = ClientCredentials::anonymous(TEST_PEER);

            // INIT: valid Int descriptor, then a truncated (2-byte) i32
            // value — a present-but-malformed pvRequest body.
            let req_desc = FieldDesc::Scalar(crate::pvdata::ScalarType::Int);
            let mut payload = Vec::new();
            payload.put_u32(sid, order);
            payload.put_u32(ioid, order);
            payload.put_u8(0x08); // INIT
            crate::pvdata::encode::encode_type_desc(&req_desc, order, &mut payload);
            payload.extend_from_slice(&[1u8, 2u8]); // truncated i32
            let cmd = match kind {
                OpKind::Monitor => Command::Monitor,
                _ => Command::Get,
            };
            let frame = synth_frame(cmd, order, payload);
            handle_op(
                &frame,
                &tx,
                &mut channels,
                order,
                &fixed_out_order(order),
                kind,
                &config,
                &mut encode_cache,
                &mut TypeCache::new(),
                peer,
                &cred,
                &discard_mon_fin(),
                &discard_exec_fin(),
            )
            .await
            .map(|()| {
                // On the (incorrect) op-error path the IOID would be
                // registered; assert it was not, to catch a silent
                // downgrade even if the call returned Ok.
                assert!(
                    !channels.get(&sid).unwrap().ops.contains_key(&ioid),
                    "a malformed INIT pvRequest must not register the IOID"
                );
            })
        }

        assert!(
            run(OpKind::Get, 800).await.is_err(),
            "a GET INIT with a truncated pvRequest value must be connection-fatal"
        );
        assert!(
            run(OpKind::Monitor, 801).await.is_err(),
            "a MONITOR INIT with a truncated pvRequest value must be connection-fatal"
        );
    }

    /// Regression: a GET/MONITOR INIT carrying a non-null descriptor that
    /// needs value bytes (scalar Int) but ENDS before them — a
    /// descriptor-only frame — is the same `from_wire_type_value` ->
    /// `!M.good()` wire fault pvxs `bev.reset()`s on (`dataencode.cpp:751`,
    /// `serverget.cpp:371-375`, `servermon.cpp:489-501`). Connection-fatal,
    /// no op reply, no IOID registration. The prior cursor-exhausted
    /// short-circuit to `Ok(None)` accepted the frame and could register
    /// the op while silently dropping the create-time options.
    #[epics_macros_rs::epics_test]
    async fn init_descriptor_only_pvrequest_value_is_connection_fatal() {
        use crate::server_native::SharedSource;
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::shared_pv::SharedPV;

        async fn run(kind: OpKind, ioid: u32) -> (bool, bool) {
            let order = ByteOrder::Little;
            let sid: u32 = 1;
            let intro = three_field_intro();
            let pv = SharedPV::new();
            pv.open(intro.clone(), three_field_value(0, 0, 0)).unwrap();
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
                    introspection: Some(std::sync::Arc::new(intro)),
                    source,
                    stat: crate::server_native::peers::ChannelStat::new(String::new()),
                    open_cred: ClientCredentials::anonymous(TEST_PEER),
                    ops: HashMap::new(),
                },
            );
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
            let config = PvaServerConfig::default();
            let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
            let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
            let cred = ClientCredentials::anonymous(TEST_PEER);

            // INIT: valid Int descriptor, then NOTHING — the value body
            // the descriptor requires is absent (descriptor-only frame).
            let req_desc = FieldDesc::Scalar(crate::pvdata::ScalarType::Int);
            let mut payload = Vec::new();
            payload.put_u32(sid, order);
            payload.put_u32(ioid, order);
            payload.put_u8(0x08); // INIT
            crate::pvdata::encode::encode_type_desc(&req_desc, order, &mut payload);
            let cmd = match kind {
                OpKind::Monitor => Command::Monitor,
                _ => Command::Get,
            };
            let frame = synth_frame(cmd, order, payload);
            let result = handle_op(
                &frame,
                &tx,
                &mut channels,
                order,
                &fixed_out_order(order),
                kind,
                &config,
                &mut encode_cache,
                &mut TypeCache::new(),
                peer,
                &cred,
                &discard_mon_fin(),
                &discard_exec_fin(),
            )
            .await;
            let fatal = result.is_err();
            let registered = channels.get(&sid).unwrap().ops.contains_key(&ioid);
            let replied = rx.try_recv().is_ok();
            (fatal, registered || replied)
        }

        let (fatal, leaked) = run(OpKind::Get, 810).await;
        assert!(fatal, "a descriptor-only GET INIT must be connection-fatal");
        assert!(
            !leaked,
            "a descriptor-only GET INIT must register no IOID and emit no op reply"
        );
        let (fatal, leaked) = run(OpKind::Monitor, 811).await;
        assert!(
            fatal,
            "a descriptor-only MONITOR INIT must be connection-fatal"
        );
        assert!(
            !leaked,
            "a descriptor-only MONITOR INIT must register no IOID and emit no op reply"
        );
    }

    /// Control: the default content-less selector (an empty `structure {}`
    /// pvRequest = select all fields) consumes zero value bytes and
    /// `from_wire_full` stays good, so a descriptor-only frame whose
    /// descriptor needs NO bytes is NOT fatal — it registers the op and
    /// replies, exactly as before. Guards the `decode_init_pv_request_value`
    /// fix against over-firing on the common default GET/MONITOR INIT.
    #[epics_macros_rs::epics_test]
    async fn init_empty_selector_descriptor_only_registers_op() {
        use crate::server_native::SharedSource;
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::shared_pv::SharedPV;

        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 820;
        let intro = three_field_intro();
        let pv = SharedPV::new();
        pv.open(intro.clone(), three_field_value(0, 0, 0)).unwrap();
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
                introspection: Some(std::sync::Arc::new(intro)),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // INIT: empty `structure {}` descriptor, no value body.
        let req_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![],
        };
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x08); // INIT
        crate::pvdata::encode::encode_type_desc(&req_desc, order, &mut payload);
        let frame = synth_frame(Command::Get, order, payload);
        let result = handle_op(
            &frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Get,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await;
        assert!(
            result.is_ok(),
            "an empty-selector descriptor-only GET INIT must not be fatal"
        );
        assert!(
            channels.get(&sid).unwrap().ops.contains_key(&ioid),
            "an empty-selector GET INIT must register the IOID"
        );
        assert!(
            rx.try_recv().is_ok(),
            "an empty-selector GET INIT must emit an op reply"
        );
    }

    /// A NULL (`0xFF`) pvRequest type descriptor at INIT is legal and means
    /// "no pvRequest": pvxs `from_wire(buf, descs, cache)` returns on
    /// `TypeCode::Null` with the buffer still GOOD (`dataencode.cpp:79-80`),
    /// `from_wire_type` yields an invalid `Value` (`:737-744`),
    /// `from_wire_type_value` skips the absent value body (`:747-753`), and
    /// the INIT passes `serverget.cpp:366-376` / `servermon.cpp:491-503`
    /// (which check only `!M.good()`). `request2mask` then takes
    /// `else if(!fields.valid()) foundrequested = true;`
    /// (`pvrequest.cpp:53-55`) and the empty mask becomes the all-fields
    /// wildcard (`:63-68`). It is the exact byte pvxs's own
    /// `to_wire(Buf&, const FieldDesc*)` writes for a null descriptor
    /// (`dataencode.cpp:29-33`).
    ///
    /// Rust rejected it as a decode error, which the read loop treats as
    /// connection-fatal — tearing down every other channel and operation
    /// multiplexed on that circuit.
    #[epics_macros_rs::epics_test]
    async fn init_null_pvrequest_descriptor_is_wildcard_not_fatal() {
        use crate::server_native::SharedSource;
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::shared_pv::SharedPV;

        /// Returns `(fatal, registered, mask)` for a one-byte `0xFF`
        /// pvRequest INIT of `kind`.
        async fn run(kind: OpKind, ioid: u32) -> (bool, bool, Option<BitSet>) {
            let order = ByteOrder::Little;
            let sid: u32 = 1;
            let intro = three_field_intro();
            let pv = SharedPV::new();
            pv.open(intro.clone(), three_field_value(0, 0, 0)).unwrap();
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
                    introspection: Some(std::sync::Arc::new(intro)),
                    source,
                    stat: crate::server_native::peers::ChannelStat::new(String::new()),
                    open_cred: ClientCredentials::anonymous(TEST_PEER),
                    ops: HashMap::new(),
                },
            );
            let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
            let config = PvaServerConfig::default();
            let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
            let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
            let cred = ClientCredentials::anonymous(TEST_PEER);

            let mut payload = Vec::new();
            payload.put_u32(sid, order);
            payload.put_u32(ioid, order);
            payload.put_u8(0x08); // INIT
            payload.put_u8(crate::pvdata::encode::TAG_NULL); // the whole pvRequest
            let cmd = match kind {
                OpKind::Monitor => Command::Monitor,
                OpKind::Rpc => Command::Rpc,
                OpKind::Put => Command::Put,
                _ => Command::Get,
            };
            let frame = synth_frame(cmd, order, payload);
            let fatal = handle_op(
                &frame,
                &tx,
                &mut channels,
                order,
                &fixed_out_order(order),
                kind,
                &config,
                &mut encode_cache,
                &mut TypeCache::new(),
                peer,
                &cred,
                &discard_mon_fin(),
                &discard_exec_fin(),
            )
            .await
            .is_err();
            let op = channels.get(&sid).unwrap().ops.get(&ioid);
            (fatal, op.is_some(), op.map(|o| o.mask.clone()))
        }

        // The wildcard mask pvxs's `request2mask` produces for an invalid
        // pvRequest: every bit of the value descriptor.
        let wildcard = BitSet::all_set(three_field_intro().total_bits());

        for (kind, ioid) in [
            (OpKind::Get, 830),
            (OpKind::Put, 831),
            (OpKind::Monitor, 832),
        ] {
            let (fatal, registered, mask) = run(kind, ioid).await;
            assert!(
                !fatal,
                "{kind:?} INIT with a NULL pvRequest must not be a wire fault"
            );
            assert!(
                registered,
                "{kind:?} INIT with a NULL pvRequest must register the IOID"
            );
            assert_eq!(
                mask.expect("op state"),
                wildcard,
                "{kind:?} INIT with a NULL pvRequest must serve the all-fields wildcard"
            );
        }

        // RPC builds no mask at all (pvxs serverget.cpp:402), so only assert
        // that it is accepted and registered.
        let (fatal, registered, _) = run(OpKind::Rpc, 833).await;
        assert!(
            !fatal,
            "RPC INIT with a NULL pvRequest must not be a wire fault"
        );
        assert!(
            registered,
            "RPC INIT with a NULL pvRequest must register the IOID"
        );
    }

    #[test]
    fn pva_r20_pipeline_bool_false_disables() {
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(false)),
            PvField::Scalar(ScalarValue::Int(16)),
        );
        let opts = parsed_opts(&req);
        assert!(!opts.enabled, "Boolean(false) must disable pipeline");
    }

    // ── ackAny → pipeline ackAt parity (servermon.cpp:554-581) ──────────

    fn make_pipeline_request_ack(value_pipe: PvField, queue: PvField, ack_any: PvField) -> PvField {
        let options = PvField::Structure(PvStructure {
            struct_id: String::new(),
            fields: vec![
                ("pipeline".to_string(), value_pipe),
                ("queueSize".to_string(), queue),
                ("ackAny".to_string(), ack_any),
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
    fn ack_at_defaults_to_one_when_ack_any_absent() {
        // pvxs MonitorOp::ackAt struct default is 1; no ackAny in the
        // pvRequest leaves it there (then clamp keeps it 1).
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(16)),
        );
        let opts = parsed_opts(&req);
        assert!(opts.enabled);
        assert_eq!(opts.ack_at, 1, "absent ackAny → ackAt == 1");
    }

    #[test]
    fn ack_at_integer_boundaries() {
        let q = 16u32;
        // Plain integer in range.
        for (ack, want) in [(3u32, 3u32), (1, 1), (16, 16)] {
            let req = make_pipeline_request_ack(
                PvField::Scalar(ScalarValue::Boolean(true)),
                PvField::Scalar(ScalarValue::Int(q as i32)),
                PvField::Scalar(ScalarValue::Int(ack as i32)),
            );
            let opts = parsed_opts(&req);
            assert_eq!(opts.ack_at, want, "ackAny={ack} with queueSize={q}");
        }
        // Explicit 0 → the minimum representable threshold, 1. CBUG-B12: pvxs
        // reads a supplied 0 as "the caller named nothing" and jumps it to
        // queueSize/2 (servermon.cpp:577-578) — this test asserted that 8.
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(q as i32)),
            PvField::Scalar(ScalarValue::Int(0)),
        );
        assert_eq!(
            parsed_opts(&req).ack_at,
            1,
            "ackAny=0 clamps up to the minimum (C++: queueSize/2)"
        );
        // Above queueSize → clamps down to queueSize (servermon.cpp:581).
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(q as i32)),
            PvField::Scalar(ScalarValue::Int(999)),
        );
        assert_eq!(
            parsed_opts(&req).ack_at,
            q,
            "ackAny>queueSize → clamp to queueSize"
        );
    }

    /// R9-32. `ackAny.as(ival)` is `tryAs<uint32_t>` — a CONVERSION, not a type
    /// test. pvxs pushes bool, every signed/unsigned integer, and both reals
    /// through one `copyOutScalar()` that C-casts to `uint64_t`
    /// (data.cpp:402-435), so a `Double(4.0)` or `Boolean(true)` ackAny reaches
    /// the plain-integer branch (`servermon.cpp:557-558`) exactly like `Int(4)`.
    ///
    /// The port matched only the integer variants and swallowed the rest in a
    /// `_ => {}` arm: `ackAt` stayed 1 (ACK every event) and the
    /// `ackAt == 0 → queueSize/2` fallback could never fire for them. One case
    /// per storage class, each landing on a value distinguishable from that
    /// stuck default.
    #[test]
    fn ack_at_converts_every_scalar_storage_class() {
        let q = 16u32;
        let cases = [
            // Real storage: `uint64_t(double)` truncates toward zero.
            (PvField::Scalar(ScalarValue::Double(4.0)), 4u32),
            (PvField::Scalar(ScalarValue::Double(4.9)), 4),
            (PvField::Scalar(ScalarValue::Float(2.0)), 2),
            // Bool storage: true → 1. The meaningful boundary is false → 0,
            // which CBUG-B12 clamps up to 1; C++ jumped it to queueSize/2, and
            // this case asserted that 8.
            (PvField::Scalar(ScalarValue::Boolean(true)), 1),
            (PvField::Scalar(ScalarValue::Boolean(false)), 1),
            // Real 0.0 lands on the same 0, so the same minimum.
            (PvField::Scalar(ScalarValue::Double(0.0)), 1),
            // Unsigned/other integer widths were already handled; pin them so
            // the exhaustive match cannot regress one.
            (PvField::Scalar(ScalarValue::UByte(3)), 3),
            (PvField::Scalar(ScalarValue::ULong(3)), 3),
            // Negative integer: pvxs wraps (`uint64_t(int64_t(-1))` → u64::MAX,
            // narrowed to 0xFFFF_FFFF) and the `[1, limit]` clamp
            // (servermon.cpp:581) pins it to the queue. The port used to
            // `u32::try_from(-1).unwrap_or(1)` → 1.
            (PvField::Scalar(ScalarValue::Int(-1)), q),
        ];
        for (ack_any, want) in cases {
            let label = format!("{ack_any:?}");
            let req = make_pipeline_request_ack(
                PvField::Scalar(ScalarValue::Boolean(true)),
                PvField::Scalar(ScalarValue::Int(q as i32)),
                ack_any,
            );
            assert_eq!(
                parsed_opts(&req).ack_at,
                want,
                "ackAny={label} with queueSize={q}"
            );
        }
    }

    #[test]
    fn ack_at_string_integer_and_garbage() {
        // Numeric string → integer path.
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(16)),
            PvField::Scalar(ScalarValue::String("4".into())),
        );
        assert_eq!(parsed_opts(&req).ack_at, 4);
        // Unparseable string → leaves the default of 1.
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(16)),
            PvField::Scalar(ScalarValue::String("garbage".into())),
        );
        assert_eq!(
            parsed_opts(&req).ack_at,
            1,
            "unparseable ackAny → default 1"
        );
    }

    /// R9-34. pvxs runs `ackAny.as(ival)` (`uint32_t`) FIRST — even for STRING
    /// storage. `copyOut` String→UInteger is `parseTo<uint64_t>`, i.e.
    /// `std::stoull(s, &idx, 0)`: BASE 0 (data.cpp:451-453, util.cpp:786-799).
    /// So a hex, octal, signed or whitespace-padded `ackAny` string resolves in
    /// the INTEGER branch, before the `"N%"` percentage form is ever considered.
    ///
    /// The port's fallback was a decimal-only `str::trim().parse::<u32>()` sitting
    /// INSIDE the string branch, so it rejected every one of these and left
    /// `ackAt` at the default of 1 (ACK every event).
    #[test]
    fn pva_r9_34_ack_any_string_converts_base_zero() {
        // queueSize 32 so none of these clamp on the way out (except -1, below).
        let q = 32u32;
        let cases: [(ScalarValue, u32); 6] = [
            // `0x`/`0X` prefix → radix 16.
            (ScalarValue::String("0x10".into()), 16),
            (ScalarValue::String("0X10".into()), 16),
            // Leading `0` → radix 8. "010" is EIGHT, not ten.
            (ScalarValue::String("010".into()), 8),
            // Plain decimal still decimal.
            (ScalarValue::String("12".into()), 12),
            // strtoull skips leading whitespace; parseTo skips trailing.
            (ScalarValue::String("  12  ".into()), 12),
            // A base-0 "0" converts to 0 — a threshold below the representable
            // minimum, so the [1, limit] clamp takes it to 1. CBUG-B12: pvxs
            // takes the `ackAt == 0 → queueSize/2` fallback (servermon.cpp:577)
            // instead, which is the 16 this case used to assert. What it still
            // pins for R9-34 is that "0" resolves in the INTEGER branch at all.
            (ScalarValue::String("0".into()), 1),
        ];
        for (ack_any, want) in cases {
            let label = format!("{ack_any:?}");
            let req = make_pipeline_request_ack(
                PvField::Scalar(ScalarValue::Boolean(true)),
                PvField::Scalar(ScalarValue::Int(q as i32)),
                PvField::Scalar(ack_any),
            );
            assert_eq!(parsed_opts(&req).ack_at, want, "ackAny={label}");
        }
        // A leading `-` negates the unsigned parse (C, not an error): "-1" is
        // u64::MAX, narrowed to 0xFFFF_FFFF, then clamped to the queue — the
        // same result the typed `Int(-1)` already produced.
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(q as i32)),
            PvField::Scalar(ScalarValue::String("-1".into())),
        );
        assert_eq!(
            parsed_opts(&req).ack_at,
            q,
            "ackAny=\"-1\" wraps to 0xFFFF_FFFF → clamps to queueSize"
        );
        // Octal radix has no digit 8: "08" is `invalid`/extraneous, so the
        // integer conversion fails, the string branch finds no `%`, and pvxs
        // leaves ackAt at the default with NO diagnostic.
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(q as i32)),
            PvField::Scalar(ScalarValue::String("08".into())),
        );
        let opts = parsed_opts(&req);
        assert_eq!(opts.ack_at, 1, "ackAny=\"08\" is not a base-0 integer");
        assert!(
            opts.diagnostics.is_empty(),
            "a non-`%` unconvertible string is silent in pvxs"
        );
    }

    /// R9-34. The percentage branch is pvxs's `else if(ackAny.as(sval))` — a
    /// CONVERSION into `std::string`, which "automagic derefs" a selected union
    /// (data.cpp:478-492). Both branches therefore see through a union-wrapped
    /// `ackAny`, which the port's `PvField::Scalar(..)` match could not.
    #[test]
    fn pva_r9_34_ack_any_derefs_a_selected_union() {
        let wrap = |sv: ScalarValue| PvField::Union {
            selector: 0,
            variant_name: "s".into(),
            value: Box::new(PvField::Scalar(sv)),
        };
        // Integer branch, through the deref.
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(16)),
            wrap(ScalarValue::String("0x4".into())),
        );
        assert_eq!(parsed_opts(&req).ack_at, 4);
        // Percentage branch, through the deref.
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(16)),
            wrap(ScalarValue::String("25%".into())),
        );
        assert_eq!(parsed_opts(&req).ack_at, 4);
    }

    #[test]
    fn ack_at_percentage_is_true_percentage_of_queue() {
        // `ackAny="N%"` is a true percentage of the queue,
        // computed as `clamp(percent,0,100) / 100 * limit` then clamped
        // to [1, limit]. (Pre-fix pvxs `servermon.cpp:563` omitted the
        // `/ 100`, saturating any percent >= 1% to the full queue.)
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(16)),
            PvField::Scalar(ScalarValue::String("50%".into())),
        );
        assert_eq!(
            parsed_opts(&req).ack_at,
            8,
            "50% of limit=16 → 8 (true percentage, not the pre-fix saturate-to-16)"
        );
        // 100% → the full queue.
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(16)),
            PvField::Scalar(ScalarValue::String("100%".into())),
        );
        assert_eq!(parsed_opts(&req).ack_at, 16, "100% → full queue");
        // 25% of 16 → 4: a clean fractional case the pre-fix formula
        // could not produce (it saturated 25*16 → clamp 16).
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(16)),
            PvField::Scalar(ScalarValue::String("25%".into())),
        );
        assert_eq!(parsed_opts(&req).ack_at, 4, "25% of 16 → 4");
        // A percent so small it truncates below one slot becomes 0, and 0 is
        // simply below the representable minimum: the `[1, limit]` floor takes
        // it to 1. CBUG-B12: pvxs instead reads that 0 as "no ackAny given" and
        // jumps it to limit/2 = 8 (servermon.cpp:577), which is what this test
        // used to assert.
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(16)),
            PvField::Scalar(ScalarValue::String("0.5%".into())),
        );
        assert_eq!(
            parsed_opts(&req).ack_at,
            1,
            "0.5% of 16 = 0.08 → floors to the minimum 1 (C++: limit/2 = 8)"
        );
    }

    /// CBUG-B12 — the mapping from requested percentage to ACK threshold is
    /// MONOTONIC NON-DECREASING. This is the property C++'s `ackAt==0` sentinel
    /// destroys: with the default limit of 4 every percentage below 25%
    /// truncates to 0 and is jumped to limit/2 = 2, so `ackAny="25%"` acks at 1
    /// while `ackAny="10%"` acks at 2 — asking to ack MORE eagerly gets a LAZIER
    /// threshold, and the flow-control window errs toward less back-pressure.
    #[test]
    fn b12_ack_at_is_monotonic_in_the_requested_percentage() {
        let q = 4u32; // the pvxs default limit, where the bug is worst
        let mut prev = 0u32;
        for pct in ["0%", "1%", "10%", "24%", "25%", "50%", "75%", "100%"] {
            let req = make_pipeline_request_ack(
                PvField::Scalar(ScalarValue::Boolean(true)),
                PvField::Scalar(ScalarValue::Int(q as i32)),
                PvField::Scalar(ScalarValue::String(pct.into())),
            );
            let got = parsed_opts(&req).ack_at;
            assert!(
                got >= prev,
                "ackAny={pct} → {got}, below the {prev} a smaller percentage got"
            );
            prev = got;
        }
        assert_eq!(prev, q, "100% → the full queue");
    }

    // pvxs `ServerConn::logRemote()` diagnostics for PRESENT-but-invalid
    // monitor `_options` (`servermon.cpp:529/542/567/572`). Asserted by
    // the option/validity boundary (the diagnostics vec carried on the
    // parsed options), not by scenario. Each assertion also confirms the
    // EFFECTIVE option value is unchanged — the diagnostics are additive.

    #[test]
    fn monitor_diag_pipeline_unparseable_string_warns() {
        // pvxs `servermon.cpp:528-530`: a PRESENT pipeline value that
        // `as(bool)` cannot parse is a Warn logRemote; pipeline stays
        // disabled (the effective value is unchanged).
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::String("garbage".into())),
            PvField::Scalar(ScalarValue::Int(16)),
        );
        let opts = parsed_opts(&req);
        assert!(!opts.enabled, "unparseable pipeline stays disabled");
        assert_eq!(opts.diagnostics.len(), 1, "one pipeline Warn");
        assert_eq!(opts.diagnostics[0].level, MessageType::Warning);
        assert!(
            opts.diagnostics[0].message.contains("pipeline"),
            "message names the pipeline option: {}",
            opts.diagnostics[0].message
        );
    }

    #[test]
    fn monitor_diag_converted_false_does_not_warn() {
        // A CONVERTED false is an intentional disable, not a parse error —
        // `as(bool)` succeeded, so pvxs emits no diagnostic. The convertible
        // false-ish values are exactly the ones `Value::as<bool>` accepts:
        // the string token `"false"` (and only that spelling — `"0"`/`"no"`/
        // `"FALSE"` are NoConvert, covered by
        // `pva_r10_31_unconvertible_pipeline_string_warns_and_disables`), plus
        // any zero numeric or `Boolean(false)`.
        for tok in [
            PvField::Scalar(ScalarValue::String("false".into())),
            PvField::Scalar(ScalarValue::Boolean(false)),
            PvField::Scalar(ScalarValue::Int(0)),
            PvField::Scalar(ScalarValue::Double(0.0)),
        ] {
            let req = make_pipeline_request(tok.clone(), PvField::Scalar(ScalarValue::Int(16)));
            let opts = parsed_opts(&req);
            assert!(!opts.enabled, "{tok:?} → disabled");
            assert!(
                opts.diagnostics.is_empty(),
                "{tok:?} converts to false, no Warn"
            );
        }
    }

    #[test]
    fn monitor_diag_non_pipeline_invalid_queue_size_warns() {
        // pvxs `servermon.cpp:541-543`: a non-pipeline monitor with a
        // PRESENT-but-invalid queueSize keeps the default depth and emits
        // a Warn logRemote ("Unable to use …").
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(false)),
            PvField::Scalar(ScalarValue::Int(1)),
        );
        let opts = parsed_opts(&req);
        assert!(!opts.enabled);
        assert_eq!(opts.queue_size, 4, "invalid queueSize → default 4");
        assert_eq!(opts.diagnostics.len(), 1, "one queueSize Warn");
        assert_eq!(opts.diagnostics[0].level, MessageType::Warning);
        assert!(
            opts.diagnostics[0].message.contains("queueSize"),
            "message names the queueSize option: {}",
            opts.diagnostics[0].message
        );
    }

    #[test]
    fn monitor_diag_valid_non_pipeline_queue_size_no_warn() {
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(false)),
            PvField::Scalar(ScalarValue::Int(16)),
        );
        assert!(
            parsed_opts(&req).diagnostics.is_empty(),
            "valid queueSize → no Warn"
        );
    }

    #[test]
    fn monitor_diag_pipeline_and_queue_size_both_warn() {
        // An unparseable pipeline AND an invalid non-pipeline queueSize are
        // both present: pvxs emits BOTH logRemote Warns.
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::String("garbage".into())),
            PvField::Scalar(ScalarValue::Int(1)),
        );
        let opts = parsed_opts(&req);
        assert!(!opts.enabled);
        assert_eq!(opts.diagnostics.len(), 2, "pipeline Warn + queueSize Warn");
        assert!(
            opts.diagnostics
                .iter()
                .all(|d| d.level == MessageType::Warning)
        );
        assert!(
            opts.diagnostics
                .iter()
                .any(|d| d.message.contains("pipeline"))
        );
        assert!(
            opts.diagnostics
                .iter()
                .any(|d| d.message.contains("queueSize"))
        );
    }

    /// R9-33. A NON-scalar `ackAny` under an enabled pipeline THROWS in pvxs:
    /// `servermon.cpp:556` runs `ackAny.as<std::string>()` ahead of both
    /// branches, and no `copyOut` arm converts array / struct / unselected-union
    /// storage into a string (`data.cpp:466-499`). Nothing catches it before
    /// `conn.cpp:277-282`, which does `bev.reset()` — the circuit is dropped.
    ///
    /// The `:570-573` "Unable to parse …" Crit is dead code (it needs BOTH
    /// conversions to fail, and anything that fails the string one has already
    /// thrown at `:556`). The port used to emit exactly that Crit and serve the
    /// monitor on with `ackAt = 1`, which is the divergence: pvxs never replies.
    #[test]
    fn pva_r9_33_non_scalar_ack_any_throws_instead_of_serving() {
        let non_scalar = [
            PvField::Structure(PvStructure {
                struct_id: String::new(),
                fields: vec![],
            }),
            PvField::ScalarArrayTyped(crate::pvdata::TypedScalarArray::Int(vec![4].into())),
            PvField::Union {
                selector: -1,
                variant_name: String::new(),
                value: Box::new(PvField::Null),
            },
        ];
        for ack_any in non_scalar {
            let label = format!("{ack_any:?}");
            let req = make_pipeline_request_ack(
                PvField::Scalar(ScalarValue::Boolean(true)),
                PvField::Scalar(ScalarValue::Int(16)),
                ack_any,
            );
            let got = negotiate_opts(&req);
            assert!(
                got.is_err(),
                "non-scalar ackAny must throw (circuit reset), not serve: {label} → {got:?}"
            );
        }
    }

    /// R9-33, the scope boundary. pvxs reads `ackAny` ONLY inside
    /// `if(op->pipeline)` (`servermon.cpp:554`), so the same unconvertible
    /// value on a NON-pipeline monitor is never touched and the INIT proceeds.
    #[test]
    fn pva_r9_33_non_scalar_ack_any_without_pipeline_is_never_read() {
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(false)),
            PvField::Scalar(ScalarValue::Int(16)),
            PvField::Structure(PvStructure {
                struct_id: String::new(),
                fields: vec![],
            }),
        );
        let opts = parsed_opts(&req);
        assert!(!opts.enabled);
        assert!(
            opts.diagnostics.is_empty(),
            "ackAny is not read without pipeline: {:?}",
            opts.diagnostics
        );
    }

    #[test]
    fn monitor_diag_ackany_bad_percent_under_pipeline_is_crit() {
        // pvxs `servermon.cpp:561-568`: a `"N%"` string whose numeric
        // prefix fails to parse → Crit "Unable to parse% …". ackAt default.
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(16)),
            PvField::Scalar(ScalarValue::String("abc%".into())),
        );
        let opts = parsed_opts(&req);
        assert_eq!(opts.ack_at, 1, "bad-percent ackAny leaves ackAt default");
        assert_eq!(opts.diagnostics.len(), 1, "one ackAny Crit");
        assert_eq!(opts.diagnostics[0].level, MessageType::Fatal);
        assert!(
            opts.diagnostics[0].message.contains("Unable to parse%"),
            "percentage-form Crit message: {}",
            opts.diagnostics[0].message
        );
    }

    #[test]
    fn monitor_diag_ackany_plain_garbage_emits_no_diagnostic() {
        // Faithful pvxs: a plain non-`%` string that fails integer parse
        // is SILENTLY ignored (`as<string>` succeeds, no `%`), so NO
        // logRemote — unlike the review doc's imprecise `ackAny=garbage`
        // Crit example. ackAt stays at the default.
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(16)),
            PvField::Scalar(ScalarValue::String("garbage".into())),
        );
        let opts = parsed_opts(&req);
        assert_eq!(opts.ack_at, 1);
        assert!(
            opts.diagnostics.is_empty(),
            "plain unparseable ackAny string → no diagnostic (pvxs silent)"
        );
    }

    #[test]
    fn monitor_diag_clean_pipeline_request_has_no_diagnostics() {
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(16)),
            PvField::Scalar(ScalarValue::Int(4)),
        );
        assert!(
            parsed_opts(&req).diagnostics.is_empty(),
            "clean request → no diagnostics"
        );
    }

    /// Emission half of the remote-monitor-log path: a
    /// MONITOR INIT carrying a PRESENT-but-invalid option must put a
    /// pvxs-shaped CMD_MESSAGE (IOID-tagged) on the wire BEFORE the INIT
    /// reply. Here an unparseable `pipeline` string yields one Warn frame.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn monitor_init_emits_remote_log_for_invalid_option() {
        use crate::server_native::SharedSource;
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::shared_pv::SharedPV;

        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 814;

        let intro = three_field_intro();
        let pv = SharedPV::new();
        pv.open(intro.clone(), three_field_value(0, 0, 0)).unwrap();

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
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // MONITOR INIT (subcmd 0x08, no pipeline bit) with an unparseable
        // `pipeline` value → pipeline disabled, one Warn logRemote.
        let req_val = make_pipeline_request(
            PvField::Scalar(ScalarValue::String("garbage".into())),
            PvField::Scalar(ScalarValue::Int(16)),
        );
        let req_desc = req_val.descriptor();
        let mut init_payload = Vec::new();
        init_payload.put_u32(sid, order);
        init_payload.put_u32(ioid, order);
        init_payload.put_u8(0x08);
        crate::pvdata::encode::encode_type_desc(&req_desc, order, &mut init_payload);
        crate::pvdata::encode::encode_pv_field(&req_val, &req_desc, order, &mut init_payload);
        let init_frame = synth_frame(Command::Monitor, order, init_payload);
        handle_op(
            &init_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("MONITOR INIT ok");

        // The FIRST frame on the wire is the diagnostic CMD_MESSAGE,
        // emitted before the INIT reply (pvxs `logRemote` precedes the
        // subscribe/reply).
        let frame = rx.recv().await.expect("a diagnostic CMD_MESSAGE frame");
        assert_eq!(
            frame[3],
            Command::Message.code(),
            "first frame must be CMD_MESSAGE, not the INIT reply"
        );
        // Payload after the 8-byte header: ioid:u32 + mtype:u8 + string.
        let got_ioid = u32::from_le_bytes([frame[8], frame[9], frame[10], frame[11]]);
        assert_eq!(got_ioid, ioid, "diagnostic is tagged with the op IOID");
        assert_eq!(
            frame[12],
            MessageType::Warning as u8,
            "unparseable pipeline → Warn (mtype 1)"
        );
        let needle = b"pipeline";
        assert!(
            frame.windows(needle.len()).any(|w| w == needle),
            "message names the pipeline option"
        );
        // The INIT reply follows the diagnostic.
        let reply = rx.recv().await.expect("INIT reply");
        assert_eq!(
            reply[3],
            Command::Monitor.code(),
            "the INIT reply is a MONITOR frame after the diagnostic"
        );
    }

    /// R9-33, on the wire. A pipelined MONITOR INIT whose `ackAny` no `copyOut`
    /// arm converts must DROP THE CIRCUIT: pvxs's `ackAny.as<std::string>()`
    /// (`servermon.cpp:556`) throws `NoConvert`, nothing catches it inside
    /// `handle_MONITOR`, and `conn.cpp:277-282` does `bev.reset()`. So there is
    /// no CMD_MESSAGE, no INIT reply, and no monitor — the connection dies.
    ///
    /// The port used to emit a Crit CMD_MESSAGE and serve the subscription on
    /// with `ackAt = 1`. `handle_op` returning `Err` is this port's `bev.reset()`
    /// (the TCP read loop tears the connection down on a `PvaError` return).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pva_r9_33_non_scalar_ack_any_resets_the_circuit() {
        use crate::server_native::SharedSource;
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::shared_pv::SharedPV;

        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 933;

        let intro = three_field_intro();
        let pv = SharedPV::new();
        pv.open(intro.clone(), three_field_value(0, 0, 0)).unwrap();
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
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // pipeline=true, a valid queueSize, and an ARRAY ackAny — `Int32A` is
        // Kind::Integer but stores as an array, and `copyOut` has no scalar arm
        // for array storage (`data.cpp:466-476`).
        let req_val = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(16)),
            PvField::ScalarArrayTyped(crate::pvdata::TypedScalarArray::Int(vec![4].into())),
        );
        let req_desc = req_val.descriptor();
        let mut init_payload = Vec::new();
        init_payload.put_u32(sid, order);
        init_payload.put_u32(ioid, order);
        init_payload.put_u8(0x88);
        crate::pvdata::encode::encode_type_desc(&req_desc, order, &mut init_payload);
        crate::pvdata::encode::encode_pv_field(&req_val, &req_desc, order, &mut init_payload);
        // The 0x80 bit obliges the initial-nack rider (`servermon.cpp:494-496`).
        init_payload.put_u32(4, order);
        let init_frame = synth_frame(Command::Monitor, order, init_payload);
        let got = handle_op(
            &init_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await;
        assert!(
            got.is_err(),
            "unconvertible ackAny must be fatal to the connection, not served: {got:?}"
        );
        assert!(
            rx.try_recv().is_err(),
            "pvxs resets before replying: no CMD_MESSAGE and no INIT reply"
        );
        assert!(
            channels[&sid].ops.is_empty(),
            "no monitor op is registered for a circuit pvxs never answers"
        );
    }

    /// pvxs `servermon.cpp:133-135` parity: the MONITOR INIT reply
    /// subcommand is derived from operation STATE (`subcmd = 0x08` for the
    /// Creating→Idle frame), NOT echoed from the request. A client that set
    /// the `0x80` pipeline bit on its INIT (`subcmd = 0x88`) must still
    /// receive a reply subcmd of exactly `0x08` — pvxs never sets `0x80` on
    /// a server→client monitor frame. (GET/PUT/RPC replies echo their inbound
    /// INIT subcmd, which is already `0x08`, so the strip is monitor-only.)
    #[epics_macros_rs::epics_test]
    async fn monitor_pipeline_init_reply_subcmd_strips_pipeline_bit() {
        use crate::server_native::SharedSource;
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::shared_pv::SharedPV;

        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 877;

        let intro = three_field_intro();
        let pv = SharedPV::new();
        pv.open(intro.clone(), three_field_value(0, 0, 0)).unwrap();
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
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // Pipeline MONITOR INIT: subcmd 0x88 (INIT | pipeline), a valid
        // pipeline pvRequest, and the trailing u32 initial-nack rider the
        // 0x80 bit requires (`servermon.cpp:494-496`).
        let req_val = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(16)),
        );
        let req_desc = req_val.descriptor();
        let mut init_payload = Vec::new();
        init_payload.put_u32(sid, order);
        init_payload.put_u32(ioid, order);
        init_payload.put_u8(0x88);
        crate::pvdata::encode::encode_type_desc(&req_desc, order, &mut init_payload);
        crate::pvdata::encode::encode_pv_field(&req_val, &req_desc, order, &mut init_payload);
        init_payload.put_u32(4, order); // initial nack (window credit)
        let init_frame = synth_frame(Command::Monitor, order, init_payload);
        handle_op(
            &init_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("pipeline MONITOR INIT ok");

        // A clean pipeline request emits no diagnostic, and the monitor data
        // task starts only on START (0x04), so the first (only) frame is the
        // INIT reply: header(8) + ioid:u32(4) + subcmd:u8 at byte 12.
        let reply = rx.recv().await.expect("INIT reply");
        assert_eq!(
            reply[3],
            Command::Monitor.code(),
            "the INIT reply is a MONITOR frame"
        );
        let got_ioid = u32::from_le_bytes([reply[8], reply[9], reply[10], reply[11]]);
        assert_eq!(got_ioid, ioid, "reply is tagged with the op IOID");
        assert_eq!(
            reply[12], 0x08,
            "the MONITOR INIT reply subcmd must be exactly 0x08 (pvxs \
             servermon.cpp:135) with the inbound 0x80 pipeline bit stripped"
        );
    }

    #[test]
    fn clamp_watermarks_caps_at_ack_at_minus_one() {
        // No pipeline (ack_at None): source levels pass through.
        assert_eq!(
            clamp_watermarks(Some((2, 8)), None),
            Some((2, 8)),
            "non-pipelined monitor uses source levels unchanged"
        );
        // No source levels: nothing to clamp.
        assert_eq!(clamp_watermarks(None, Some(5)), None);
        // ack_at=5 → cap=4: low(2) stays, high(8) capped to 4.
        assert_eq!(clamp_watermarks(Some((2, 8)), Some(5)), Some((2, 4)));
        // ack_at=1 (pvxs default) → cap=0: both marks forced to 0,
        // matching pvxs `min(low, ackAt-1)` with ackAt=1.
        assert_eq!(clamp_watermarks(Some((2, 8)), Some(1)), Some((0, 0)));
        // Gateway returns (0,0): clamp is a no-op regardless of ack_at,
        // so ackAt is observable only for sources with high>0 marks.
        assert_eq!(clamp_watermarks(Some((0, 0)), Some(7)), Some((0, 0)));
    }

    fn test_channel_ctx() -> crate::server_native::source::ChannelContext {
        crate::server_native::source::ChannelContext {
            peer: "127.0.0.1:5075".parse().unwrap(),
            account: String::new(),
            method: "anonymous".to_string(),
            host: String::new(),
            authority: String::new(),
            roles: Vec::new(),
            pv_request: None,
            log: Default::default(),
        }
    }

    /// Owner path: `MonitorPipelineCredit::take` consumes exactly one window
    /// slot per call, and `available()` reports the gate the emit arm reads.
    #[epics_macros_rs::epics_test]
    async fn monitor_pipeline_credit_take_decrements_window() {
        use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
        let window = Arc::new(AtomicU32::new(2));
        let notify = Arc::new(tokio::sync::Notify::new());
        let wm_seq = Arc::new(AtomicU64::new(1));
        let src: DynSource = Arc::new(crate::server_native::SharedSource::new());
        let ctx = test_channel_ctx();
        let credit = MonitorPipelineCredit {
            window: Some(&window),
            window_notify: Some(&notify),
            wm_levels: None,
            wm_seq: &wm_seq,
            monitor_op_id: 1,
            src: &src,
            pv_name: "dut",
            mon_ctx: &ctx,
        };
        assert!(credit.available());
        credit.take();
        assert_eq!(window.load(Ordering::Relaxed), 1);
        assert!(credit.available());
        credit.take();
        assert_eq!(window.load(Ordering::Relaxed), 0);
        assert!(
            !credit.available(),
            "an exhausted window closes the emit gate"
        );
    }

    /// R12-33 — an exhausted window must SUPPRESS THE REPLY, not the drain.
    /// pvxs `maybeReply` simply does not fire while `window == 0`
    /// (`servermon.cpp:79-83`) and `doPost` goes on squashing into the
    /// negotiated queue. So the credit primitive must be a non-blocking gate
    /// (`available`) plus a wake-up (`arm_refill`) that the event loop can
    /// select over alongside `rx.recv()` — never an await that parks the loop
    /// and stops draining the source.
    ///
    /// Also pins the arm-before-read ordering: the ACK path adds credit and
    /// calls `notify_waiters()`, which stores no permit, so a waiter armed
    /// AFTER the window read would miss the refill and park forever.
    #[epics_macros_rs::epics_test]
    async fn monitor_pipeline_credit_refill_wakes_the_armed_waiter() {
        use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
        let window = Arc::new(AtomicU32::new(0));
        let notify = Arc::new(tokio::sync::Notify::new());
        let wm_seq = Arc::new(AtomicU64::new(1));
        let src: DynSource = Arc::new(crate::server_native::SharedSource::new());
        let ctx = test_channel_ctx();
        let credit = MonitorPipelineCredit {
            window: Some(&window),
            window_notify: Some(&notify),
            wm_levels: None,
            wm_seq: &wm_seq,
            monitor_op_id: 1,
            src: &src,
            pv_name: "dut",
            mon_ctx: &ctx,
        };

        // Exhausted: the gate is shut and the refill waiter parks.
        let refill = credit.arm_refill();
        assert!(!credit.available(), "no credit → the emit gate is shut");
        assert!(
            epics_base_rs::runtime::task::timeout(
                Duration::from_millis(50),
                wait_credit_refill(refill)
            )
            .await
            .is_err(),
            "with no ACK the refill waiter must park"
        );

        // Arm, THEN let the ACK land: `notify_waiters()` leaves no permit, so
        // only an already-registered waiter is woken. This is the ordering the
        // emit loop relies on.
        let refill = credit.arm_refill();
        window.fetch_add(1, Ordering::Relaxed);
        notify.notify_waiters();
        assert!(
            epics_base_rs::runtime::task::timeout(
                Duration::from_millis(500),
                wait_credit_refill(refill)
            )
            .await
            .is_ok(),
            "an ACK refill must wake the armed waiter"
        );
        assert!(credit.available(), "the refilled window re-opens the gate");
        credit.take();
        assert_eq!(window.load(Ordering::Relaxed), 0);
    }

    /// Owner path: a non-pipeline monitor (no window) is always emit-eligible,
    /// never waits, and touches no counter.
    #[epics_macros_rs::epics_test]
    async fn monitor_pipeline_credit_no_window_is_no_op() {
        use std::sync::atomic::AtomicU64;
        let wm_seq = Arc::new(AtomicU64::new(1));
        let src: DynSource = Arc::new(crate::server_native::SharedSource::new());
        let ctx = test_channel_ctx();
        let credit = MonitorPipelineCredit {
            window: None,
            window_notify: None,
            wm_levels: None,
            wm_seq: &wm_seq,
            monitor_op_id: 1,
            src: &src,
            pv_name: "dut",
            mon_ctx: &ctx,
        };
        assert!(
            credit.available(),
            "a non-pipeline monitor is never credit-blocked"
        );
        credit.take();
        assert!(
            credit.arm_refill().is_none(),
            "no window → nothing to wait on"
        );
    }

    /// Bypass path (the formerly-uncounted send site): the pipeline
    /// initial snapshot must consume a window credit, exactly like every
    /// subsequent DATA frame (pvxs `servermon.cpp:192`). With
    /// `queueSize=2` and NO client ACK, the server may send at most 2
    /// DATA frames before it stalls. The bug let the initial snapshot
    /// ride out free, so the server sent 3 — the window drifted to
    /// `queueSize + 1`, one more than the client's queue could hold.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pipeline_initial_snapshot_consumes_one_credit() {
        use crate::server_native::SharedSource;
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::shared_pv::SharedPV;

        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 700;

        let intro = three_field_intro();
        let pv = SharedPV::new();
        pv.open(intro.clone(), three_field_value(0, 0, 0)).unwrap();
        let pusher = pv.clone();

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
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // MONITOR INIT with pipeline=true, queueSize=2 and an explicit
        // initial nack rider of 2 (subcmd 0x88 sets the 0x80 pipeline bit,
        // a u32 `nack` follows the pvRequest) — the window initialises to
        // the rider value 2. An ABSENT rider seeds the window to 0
        // (covered by `pipeline_absent_nack_stalls_until_ack`).
        let req_val = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(2)),
        );
        let req_desc = req_val.descriptor();
        let mut init_payload = Vec::new();
        init_payload.put_u32(sid, order);
        init_payload.put_u32(ioid, order);
        init_payload.put_u8(0x88);
        crate::pvdata::encode::encode_type_desc(&req_desc, order, &mut init_payload);
        crate::pvdata::encode::encode_pv_field(&req_val, &req_desc, order, &mut init_payload);
        init_payload.put_u32(2, order);
        let init_frame = synth_frame(Command::Monitor, order, init_payload);
        handle_op(
            &init_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("MONITOR INIT ok");
        let _ = rx.recv().await.expect("INIT reply");

        // MONITOR START (subcmd 0x44 = start | process) spawns the
        // subscriber task.
        let mut start_payload = Vec::new();
        start_payload.put_u32(sid, order);
        start_payload.put_u32(ioid, order);
        start_payload.put_u8(0x44);
        let start_frame = synth_frame(Command::Monitor, order, start_payload);
        handle_op(
            &start_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("MONITOR START ok");

        // Let the task subscribe and emit the initial snapshot plus the
        // SharedPV-queued initial event, draining the 2-slot window.
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Push updates with NO ACK. The window is exhausted, so none can
        // produce a DATA frame.
        for i in 1..=8 {
            pusher.try_post(three_field_value(i, 0, 0));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;

        let mut data_frames = 0usize;
        while rx.try_recv().is_ok() {
            data_frames += 1;
        }
        assert_eq!(
            data_frames, 2,
            "with queueSize=2 and no ACK the server must send exactly 2 DATA \
             frames (the initial snapshot consumes one credit); got \
             {data_frames} — the initial snapshot bypassed the pipeline window"
        );
    }

    // ================================================================
    // Monitor accrual: the MONITOR subscriber is spawned at INIT (pvxs
    // `onSubscribe`, servermon.cpp:591), so posts arriving in the
    // INIT->START window accrue into the op's bounded FIFO instead of
    // being lost; the consumer emits `pending -> wire` ONLY while
    // Executing (after a START, before a STOP). INIT->START and
    // STOP->START are the same "Idle, accruing" state. These tests
    // drive the real `handle_op` INIT-spawn + START/STOP/DESTROY
    // state-flip and decode the wire frames, one case per invariant
    // boundary.
    // ================================================================

    /// Build a MONITOR INIT frame for a NON-pipeline monitor with the
    /// given `queueSize` (the per-op squash bound). Non-pipeline so no
    /// credit window gates emission — the accrued backlog flushes freely
    /// at START, isolating the FIFO bound from pipeline crediting.
    #[cfg(test)]
    fn pvx61_init_frame(sid: u32, ioid: u32, queue_size: i32, order: ByteOrder) -> Frame {
        let req_val = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(false)),
            PvField::Scalar(ScalarValue::Int(queue_size)),
        );
        let req_desc = req_val.descriptor();
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x08); // INIT (no 0x80 pipeline bit)
        crate::pvdata::encode::encode_type_desc(&req_desc, order, &mut payload);
        crate::pvdata::encode::encode_pv_field(&req_val, &req_desc, order, &mut payload);
        synth_frame(Command::Monitor, order, payload)
    }

    /// Build a data-phase MONITOR control frame: START `0x44`
    /// (`0x04` start/stop | `0x40` start), STOP `0x04`, DESTROY `0x10`.
    #[cfg(test)]
    fn pvx61_ctrl_frame(sid: u32, ioid: u32, subcmd: u8, order: ByteOrder) -> Frame {
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(subcmd);
        synth_frame(Command::Monitor, order, payload)
    }

    /// Single-PV `SharedSource` channel map for the monitor-accrual tests, seeded
    /// with `three_field_value(0, 0, 0)`. Returns the channels map, the
    /// source (kept alive by the caller), and a clonable pusher for
    /// post-INIT posts.
    #[cfg(test)]
    fn pvx61_channels(
        sid: u32,
        intro: &FieldDesc,
    ) -> (
        HashMap<u32, ChannelState>,
        DynSource,
        crate::server_native::shared_pv::SharedPV,
    ) {
        let pv = crate::server_native::shared_pv::SharedPV::new();
        pv.open(intro.clone(), three_field_value(0, 0, 0)).unwrap();
        let pusher = pv.clone();
        let shared = crate::server_native::SharedSource::new();
        shared.add("dut", pv);
        let source: DynSource = Arc::new(shared);
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );
        (channels, source, pusher)
    }

    /// Drive one MONITOR frame through `handle_op` with the monitor-accrual
    /// test defaults (fixed byte order, throwaway decode cache + exec-fin).
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    async fn pvx61_drive(
        frame: &Frame,
        tx: &SrvTx,
        channels: &mut HashMap<u32, ChannelState>,
        order: ByteOrder,
        config: &PvaServerConfig,
        encode_cache: &mut crate::pvdata::encode::EncodeTypeCache,
        peer: SocketAddr,
        cred: &ClientCredentials,
        mon_fin: &mpsc::UnboundedSender<MonitorFinished>,
    ) -> PvaResult<()> {
        handle_op(
            frame,
            tx,
            channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            config,
            encode_cache,
            &mut TypeCache::new(),
            peer,
            cred,
            mon_fin,
            &discard_exec_fin(),
        )
        .await
    }

    /// Decode the field-`a` value of a MONITOR DATA frame (subcmd
    /// `0x00`), or `None` for any non-DATA frame (INIT reply `0x08`,
    /// FINISH `0x10`). `SharedSource` marks no leaves (`MonitorUpdate::
    /// marked == None`), so every DATA frame carries all three fields and
    /// `three_field_extract` applies.
    #[cfg(test)]
    fn pvx61_decode_data_a(resp: &[u8], intro: &FieldDesc, order: ByteOrder) -> Option<i32> {
        let (frame, _) = try_parse_frame(resp).ok()??;
        if frame.header.command != Command::Monitor.code() {
            return None;
        }
        let mut cur = frame.cursor();
        let _ioid = cur.get_u32(order).ok()?;
        let subcmd = cur.get_u8().ok()?;
        if subcmd != 0x00 {
            return None; // INIT reply / FINISH — not a DATA frame
        }
        let changed = BitSet::decode(&mut cur, order).ok()?;
        let value =
            crate::pvdata::encode::decode_pv_field_with_bitset(intro, &changed, 0, &mut cur, order)
                .ok()?;
        Some(three_field_extract(&value).0)
    }

    /// Drain every currently-queued frame from `rx` and return the
    /// field-`a` sequence of the DATA frames (INIT reply + FINISH are
    /// filtered out).
    #[cfg(test)]
    fn pvx61_drain_data_a(
        rx: &mut mpsc::Receiver<Vec<u8>>,
        intro: &FieldDesc,
        order: ByteOrder,
    ) -> Vec<i32> {
        let mut out = Vec::new();
        while let Ok(buf) = rx.try_recv() {
            if let Some(a) = pvx61_decode_data_a(&buf, intro, order) {
                out.push(a);
            }
        }
        out
    }

    /// True if `resp` is a MONITOR FINISH frame (subcmd `0x10`).
    #[cfg(test)]
    fn pvx61_is_finish(resp: &[u8], order: ByteOrder) -> bool {
        let Ok(Some((frame, _))) = try_parse_frame(resp) else {
            return false;
        };
        if frame.header.command != Command::Monitor.code() {
            return false;
        }
        let mut cur = frame.cursor();
        if cur.get_u32(order).is_err() {
            return false;
        }
        matches!(cur.get_u8(), Ok(0x10))
    }

    /// INIT->START accrual + ordering. Posts B, C, D arriving in the
    /// INIT->START window are delivered in order (A(seed), B, C, D) at the
    /// first START. Pre-fix the subscriber spawned only at START, so the
    /// window posts were lost and only the latest seed survived.
    #[epics_macros_rs::epics_test]
    async fn pvx61_init_window_posts_accrue_in_order() {
        let order = ByteOrder::Little;
        let (sid, ioid) = (1u32, 700u32);
        let intro = three_field_intro();
        let (mut channels, _source, pusher) = pvx61_channels(sid, &intro);
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // INIT with queueSize 8 — large enough to hold the seed + 3 posts
        // with no squash.
        pvx61_drive(
            &pvx61_init_frame(sid, ioid, 8, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("MONITOR INIT ok");
        let _ = rx.recv().await.expect("INIT introspection reply");

        // Let the subscriber task subscribe and capture the seed (0,0,0)
        // BEFORE any post arrives.
        epics_base_rs::runtime::task::sleep(Duration::from_millis(150)).await;
        // Posts arrive while the monitor is Idle (INIT->START window).
        for i in 1..=3 {
            pusher.try_post(three_field_value(i, 0, 0));
            epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
        // Invariant: an Idle monitor emits nothing before START.
        assert!(
            rx.try_recv().is_err(),
            "an Idle (pre-START) monitor must not emit any DATA frame"
        );

        // START flips Executing; the accrued backlog flushes in order.
        pvx61_drive(
            &pvx61_ctrl_frame(sid, ioid, 0x44, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("MONITOR START ok");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(250)).await;

        let seq = pvx61_drain_data_a(&mut rx, &intro, order);
        assert_eq!(
            seq,
            vec![0, 1, 2, 3],
            "INIT->START posts must be delivered in order at START (pre-fix \
             only the latest seed survived); got {seq:?}"
        );
    }

    /// Backlog > queueSize bounds to queueSize. With queueSize 2,
    /// five posts in the INIT->START window squash to the seed + the
    /// latest, so START delivers exactly 2 frames (bounded FIFO, newest
    /// tail wins).
    #[epics_macros_rs::epics_test]
    async fn pvx61_backlog_bounds_to_queue_size() {
        let order = ByteOrder::Little;
        let (sid, ioid) = (2u32, 701u32);
        let intro = three_field_intro();
        let (mut channels, _source, pusher) = pvx61_channels(sid, &intro);
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        pvx61_drive(
            &pvx61_init_frame(sid, ioid, 2, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("MONITOR INIT ok");
        let _ = rx.recv().await.expect("INIT introspection reply");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(150)).await;

        // Five posts while Idle — the FIFO (seed + queueSize-1 tail) must
        // bound them to 2 total.
        for i in 1..=5 {
            pusher.try_post(three_field_value(i, 0, 0));
            epics_base_rs::runtime::task::sleep(Duration::from_millis(15)).await;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(150)).await;

        pvx61_drive(
            &pvx61_ctrl_frame(sid, ioid, 0x44, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("MONITOR START ok");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(250)).await;

        let seq = pvx61_drain_data_a(&mut rx, &intro, order);
        assert_eq!(
            seq.len(),
            2,
            "queueSize=2 must bound the INIT->START backlog to exactly 2 \
             frames; got {seq:?}"
        );
        assert_eq!(seq[0], 0, "first frame is the connect-time seed");
        assert_eq!(
            *seq.last().unwrap(),
            5,
            "the squashed tail must carry the latest post"
        );
    }

    /// Backlog == 0: a START with no window posts delivers the seed
    /// only.
    #[epics_macros_rs::epics_test]
    async fn pvx61_seed_only_no_backlog() {
        let order = ByteOrder::Little;
        let (sid, ioid) = (3u32, 702u32);
        let intro = three_field_intro();
        let (mut channels, _source, _pusher) = pvx61_channels(sid, &intro);
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        pvx61_drive(
            &pvx61_init_frame(sid, ioid, 8, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("MONITOR INIT ok");
        let _ = rx.recv().await.expect("INIT introspection reply");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(150)).await;

        pvx61_drive(
            &pvx61_ctrl_frame(sid, ioid, 0x44, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("MONITOR START ok");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;

        let seq = pvx61_drain_data_a(&mut rx, &intro, order);
        assert_eq!(
            seq,
            vec![0],
            "with no window posts the START delivers only the seed; got {seq:?}"
        );
    }

    /// Backlog == 1: a single window post delivers the seed + that
    /// one post.
    #[epics_macros_rs::epics_test]
    async fn pvx61_single_backlog() {
        let order = ByteOrder::Little;
        let (sid, ioid) = (4u32, 703u32);
        let intro = three_field_intro();
        let (mut channels, _source, pusher) = pvx61_channels(sid, &intro);
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        pvx61_drive(
            &pvx61_init_frame(sid, ioid, 8, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("MONITOR INIT ok");
        let _ = rx.recv().await.expect("INIT introspection reply");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(150)).await;

        pusher.try_post(three_field_value(1, 0, 0));
        epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;

        pvx61_drive(
            &pvx61_ctrl_frame(sid, ioid, 0x44, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("MONITOR START ok");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;

        let seq = pvx61_drain_data_a(&mut rx, &intro, order);
        assert_eq!(
            seq,
            vec![0, 1],
            "one window post delivers the seed then that post; got {seq:?}"
        );
    }

    /// STOP->START depth (the accrual sibling). STOP->START is the
    /// same Idle-accruing state as INIT->START: posts during the pause
    /// accrue up to queueSize and flush at the next START. Pre-fix the
    /// single `held` cell delivered only the latest pause-window value.
    #[epics_macros_rs::epics_test]
    async fn pvx61_stop_start_delivers_backlog_depth() {
        let order = ByteOrder::Little;
        let (sid, ioid) = (5u32, 704u32);
        let intro = three_field_intro();
        let (mut channels, _source, pusher) = pvx61_channels(sid, &intro);
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        pvx61_drive(
            &pvx61_init_frame(sid, ioid, 8, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("MONITOR INIT ok");
        let _ = rx.recv().await.expect("INIT introspection reply");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(150)).await;

        // First START drains the seed.
        pvx61_drive(
            &pvx61_ctrl_frame(sid, ioid, 0x44, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("first MONITOR START ok");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(150)).await;
        let seed = pvx61_drain_data_a(&mut rx, &intro, order);
        assert_eq!(seed, vec![0], "first START delivers the seed; got {seed:?}");

        // STOP → Idle. Posts during the pause accrue.
        pvx61_drive(
            &pvx61_ctrl_frame(sid, ioid, 0x04, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("MONITOR STOP ok");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
        for i in 1..=3 {
            pusher.try_post(three_field_value(i, 0, 0));
            epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
        // Still Idle — nothing emitted during the pause.
        assert!(
            rx.try_recv().is_err(),
            "a STOPped monitor must not emit during the pause window"
        );

        // Second START flushes the pause backlog up to queueSize.
        pvx61_drive(
            &pvx61_ctrl_frame(sid, ioid, 0x44, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("second MONITOR START ok");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(250)).await;

        let seq = pvx61_drain_data_a(&mut rx, &intro, order);
        assert_eq!(
            seq,
            vec![1, 2, 3],
            "STOP->START must deliver the full pause backlog up to queueSize \
             (pre-fix the single `held` cell delivered only the latest); got {seq:?}"
        );
    }

    /// DESTROY before START tears the upstream down. The subscriber
    /// task is spawned at INIT with its abort guard installed in the same
    /// synchronous step, so a DESTROY arriving before any START removes
    /// the op, drops the abort, and fires the terminal `MonitorFinished`.
    #[epics_macros_rs::epics_test]
    async fn pvx61_destroy_before_start_tears_down() {
        let order = ByteOrder::Little;
        let (sid, ioid) = (6u32, 705u32);
        let intro = three_field_intro();
        let (mut channels, _source, _pusher) = pvx61_channels(sid, &intro);
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);
        let (mon_fin_tx, mut mon_fin_rx) = mpsc::unbounded_channel::<MonitorFinished>();

        pvx61_drive(
            &pvx61_init_frame(sid, ioid, 8, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &mon_fin_tx,
        )
        .await
        .expect("MONITOR INIT ok");
        let _ = rx.recv().await.expect("INIT introspection reply");
        // Let the task subscribe and install its `MonitorFinishGuard`.
        epics_base_rs::runtime::task::sleep(Duration::from_millis(150)).await;
        assert!(
            mon_fin_rx.try_recv().is_err(),
            "the subscriber is alive (Idle) before DESTROY — no finish yet"
        );

        // DESTROY (0x10) before any START.
        pvx61_drive(
            &pvx61_ctrl_frame(sid, ioid, 0x10, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &mon_fin_tx,
        )
        .await
        .expect("MONITOR DESTROY ok");

        let fin = epics_base_rs::runtime::task::timeout(Duration::from_secs(2), mon_fin_rx.recv())
            .await
            .expect("teardown fires within 2s")
            .expect("MonitorFinished");
        assert_eq!(fin.ioid, ioid, "the torn-down op is the DESTROYed one");
        assert!(
            !channels[&sid].ops.contains_key(&ioid),
            "DESTROY removed the op from ch.ops"
        );
    }

    /// Never-START tears down on connection drop. INIT spawns the
    /// subscriber; dropping the channels map (connection teardown) before
    /// any START drops the op's abort guard, aborting the task and firing
    /// the terminal `MonitorFinished`.
    #[epics_macros_rs::epics_test]
    async fn pvx61_never_start_tears_down_on_drop() {
        let order = ByteOrder::Little;
        let (sid, ioid) = (7u32, 706u32);
        let intro = three_field_intro();
        let (mut channels, _source, _pusher) = pvx61_channels(sid, &intro);
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);
        let (mon_fin_tx, mut mon_fin_rx) = mpsc::unbounded_channel::<MonitorFinished>();

        pvx61_drive(
            &pvx61_init_frame(sid, ioid, 8, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &mon_fin_tx,
        )
        .await
        .expect("MONITOR INIT ok");
        let _ = rx.recv().await.expect("INIT introspection reply");
        // Let the task subscribe and install its guard before teardown.
        epics_base_rs::runtime::task::sleep(Duration::from_millis(150)).await;

        // Connection torn down before any START.
        drop(channels);

        let fin = epics_base_rs::runtime::task::timeout(Duration::from_secs(2), mon_fin_rx.recv())
            .await
            .expect("teardown fires within 2s")
            .expect("MonitorFinished");
        assert_eq!(fin.ioid, ioid, "the never-started op is torn down on drop");
    }

    /// Source-close while Idle holds the backlog AND the finish. An
    /// Idle/never-STARTed monitor whose PV closes must NOT emit MONITOR
    /// FINISH — pvxs gates emission on `state==Executing` and holds both the
    /// backlog and the finish until a later START (servermon.cpp:82,142-154).
    /// Pre-fix the terminal FINISH was ungated on Executing, so an Idle-close
    /// emitted FINISH immediately and abandoned the accrued backlog. Newly
    /// reachable because the subscriber now runs from INIT.
    #[epics_macros_rs::epics_test]
    async fn pvx61_source_close_while_idle_holds_backlog_until_start() {
        let order = ByteOrder::Little;
        let (sid, ioid) = (11u32, 710u32);
        let intro = three_field_intro();
        let (mut channels, _source, pusher) = pvx61_channels(sid, &intro);
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        pvx61_drive(
            &pvx61_init_frame(sid, ioid, 8, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("MONITOR INIT ok");
        let _ = rx.recv().await.expect("INIT introspection reply");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(150)).await;

        // Posts arrive while the monitor is Idle (INIT->START window).
        for i in 1..=3 {
            pusher.try_post(three_field_value(i, 0, 0));
            epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;

        // The PV closes while the monitor is STILL Idle (never STARTed): the
        // subscriber's source channel ends but no START has arrived.
        pusher.close();
        epics_base_rs::runtime::task::sleep(Duration::from_millis(150)).await;

        // Invariant: an Idle monitor emits NOTHING on source-close — no
        // FINISH, no DATA. The backlog and the finish are held for a START.
        assert!(
            rx.try_recv().is_err(),
            "an Idle (pre-START) monitor must not emit FINISH when the source \
             closes — the backlog and finish are held until a START"
        );

        // START flips Executing: the held backlog flushes in order, THEN the
        // terminal FINISH follows.
        pvx61_drive(
            &pvx61_ctrl_frame(sid, ioid, 0x44, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("MONITOR START ok");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(250)).await;

        // Classify every drained frame in order: DATA field-`a` values, and
        // whether the terminal FINISH followed the whole backlog.
        let mut data = Vec::new();
        let mut saw_finish = false;
        let mut finish_after_all_data = true;
        while let Ok(buf) = rx.try_recv() {
            if let Some(a) = pvx61_decode_data_a(&buf, &intro, order) {
                if saw_finish {
                    finish_after_all_data = false; // DATA after FINISH — wrong order
                }
                data.push(a);
            } else if pvx61_is_finish(&buf, order) {
                saw_finish = true;
            }
        }
        assert_eq!(
            data,
            vec![0, 1, 2, 3],
            "the backlog held across the Idle source-close must flush in order \
             at START (pre-fix it was dropped); got {data:?}"
        );
        assert!(
            saw_finish,
            "MONITOR FINISH must follow the flushed backlog once the source is \
             closed and the monitor reaches Executing"
        );
        assert!(
            finish_after_all_data,
            "FINISH must arrive AFTER the whole backlog, not interleaved"
        );
    }

    /// Build a raw MONITOR DATA event (`changed | value | overrun`) for
    /// the three-Int structure with field `a = a`: full changed mask,
    /// empty overrun trailer — the shape a same-endian raw forward
    /// carries to the wire verbatim.
    #[cfg(test)]
    fn pvx61_raw_event(a: i32, order: ByteOrder) -> crate::server_native::RawMonitorEvent {
        let intro = three_field_intro();
        let changed = BitSet::all_set(intro.total_bits());
        let value = three_field_value(a, 0, 0);
        let mut body = Vec::new();
        changed.write_into(order, &mut body);
        crate::pvdata::encode::encode_pv_field_with_bitset(
            &value, &intro, &changed, 0, order, &mut body,
        );
        BitSet::new().write_into(order, &mut body); // required empty overrun trailer
        crate::server_native::RawMonitorEvent {
            body_bytes: bytes::Bytes::from(body),
            byte_order: order,
            type_changed: false,
        }
    }

    /// A source exposing the RAW monitor fast path: `subscribe_raw` hands
    /// out a pre-built receiver the test pushes `RawMonitorEvent`s into,
    /// and `get_value` supplies the cooked seed. Full mask + no pipeline
    /// window + no filters ⇒ the subscriber takes the raw path.
    #[cfg(test)]
    struct RawSeedSource {
        intro: FieldDesc,
        seed: PvField,
        raw_rx: std::sync::Mutex<Option<MonitorStream<crate::server_native::RawMonitorEvent>>>,
    }

    #[cfg(test)]
    impl crate::server_native::source::ChannelSource for RawSeedSource {
        async fn list_pvs(&self) -> Vec<String> {
            vec!["dut".into()]
        }
        async fn has_pv(&self, name: &str) -> bool {
            name == "dut"
        }
        async fn get_introspection(&self, _name: &str) -> Option<FieldDesc> {
            Some(self.intro.clone())
        }
        async fn get_value(&self, _name: &str) -> Option<PvField> {
            Some(self.seed.clone())
        }
        async fn put_value(&self, _name: &str, _v: PvField) -> Result<(), OpError> {
            Ok(())
        }
        async fn is_writable(&self, _name: &str) -> bool {
            false
        }
        async fn subscribe(&self, _name: &str) -> Option<MonitorStream<PvField>> {
            None
        }
        async fn subscribe_raw(
            &self,
            _name: &str,
        ) -> Option<MonitorStream<crate::server_native::RawMonitorEvent>> {
            self.raw_rx.lock().unwrap().take()
        }
    }

    /// Single-PV `RawSeedSource` channel map, seeded with
    /// `three_field_value(0, 0, 0)`. Returns the channels map, the source,
    /// and the raw-event pusher.
    #[cfg(test)]
    fn pvx61_raw_channels(
        sid: u32,
        intro: &FieldDesc,
    ) -> (
        HashMap<u32, ChannelState>,
        DynSource,
        mpsc::Sender<crate::server_native::RawMonitorEvent>,
    ) {
        let (raw_tx, raw_rx) = mpsc::channel::<crate::server_native::RawMonitorEvent>(64);
        let source: DynSource = Arc::new(RawSeedSource {
            intro: intro.clone(),
            seed: three_field_value(0, 0, 0),
            raw_rx: std::sync::Mutex::new(Some(raw_rx.into())),
        });
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );
        (channels, source, raw_tx)
    }

    /// Raw fast path: INIT->START posts accrue in order (the raw
    /// counterpart of T1). The raw subscriber accrues events from INIT and
    /// flushes them after START in order, ahead of the cooked seed's
    /// backlog.
    #[epics_macros_rs::epics_test]
    async fn pvx61_raw_path_accrues_in_order() {
        let order = ByteOrder::Little;
        let (sid, ioid) = (8u32, 707u32);
        let intro = three_field_intro();
        let (mut channels, _source, raw_tx) = pvx61_raw_channels(sid, &intro);
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        pvx61_drive(
            &pvx61_init_frame(sid, ioid, 8, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("MONITOR INIT ok");
        let _ = rx.recv().await.expect("INIT introspection reply");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(150)).await;

        for i in 1..=3 {
            raw_tx
                .send(pvx61_raw_event(i, order))
                .await
                .expect("raw event queued");
            epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
        assert!(
            rx.try_recv().is_err(),
            "an Idle (pre-START) raw monitor must not emit any DATA frame"
        );

        pvx61_drive(
            &pvx61_ctrl_frame(sid, ioid, 0x44, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("MONITOR START ok");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(250)).await;

        let seq = pvx61_drain_data_a(&mut rx, &intro, order);
        assert_eq!(
            seq,
            vec![0, 1, 2, 3],
            "raw INIT->START events must be delivered seed-first, then in order; got {seq:?}"
        );
    }

    /// Raw fast path: backlog > queueSize bounds to queueSize (the
    /// raw counterpart of T2). Five events with queueSize 2 flush to
    /// exactly 2 frames (seed + latest), the same total the decoded path
    /// yields — the cooked seed counts against queueSize on both paths.
    #[epics_macros_rs::epics_test]
    async fn pvx61_raw_path_bounds_to_queue_size() {
        let order = ByteOrder::Little;
        let (sid, ioid) = (9u32, 708u32);
        let intro = three_field_intro();
        let (mut channels, _source, raw_tx) = pvx61_raw_channels(sid, &intro);
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        pvx61_drive(
            &pvx61_init_frame(sid, ioid, 2, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("MONITOR INIT ok");
        let _ = rx.recv().await.expect("INIT introspection reply");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(150)).await;

        for i in 1..=5 {
            raw_tx
                .send(pvx61_raw_event(i, order))
                .await
                .expect("raw event queued");
            epics_base_rs::runtime::task::sleep(Duration::from_millis(15)).await;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(150)).await;

        pvx61_drive(
            &pvx61_ctrl_frame(sid, ioid, 0x44, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("MONITOR START ok");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(250)).await;

        let seq = pvx61_drain_data_a(&mut rx, &intro, order);
        assert_eq!(
            seq.len(),
            2,
            "queueSize=2 must bound the raw INIT->START backlog to exactly 2 \
             frames (seed counts against the bound); got {seq:?}"
        );
        assert_eq!(seq[0], 0, "first frame is the cooked seed");
        assert_eq!(
            *seq.last().unwrap(),
            5,
            "the squashed raw tail must carry the latest event"
        );
    }

    /// Raw-seeded source whose ACL gate can be flipped from permissive to
    /// deny mid-window (the test holds the ACF cell + version counter), to
    /// exercise the raw seed's per-event ACL recheck.
    #[cfg(test)]
    struct FlipRawSeedSource {
        intro: FieldDesc,
        seed: PvField,
        raw_rx: std::sync::Mutex<Option<MonitorStream<crate::server_native::RawMonitorEvent>>>,
        gate: epics_base_rs::server::access_security::AccessGate,
    }

    #[cfg(test)]
    impl crate::server_native::source::ChannelSource for FlipRawSeedSource {
        fn access(&self) -> &epics_base_rs::server::access_security::AccessGate {
            &self.gate
        }
        async fn list_pvs(&self) -> Vec<String> {
            vec!["dut".into()]
        }
        async fn has_pv(&self, name: &str) -> bool {
            name == "dut"
        }
        async fn get_introspection(&self, _name: &str) -> Option<FieldDesc> {
            Some(self.intro.clone())
        }
        async fn get_value(&self, _name: &str) -> Option<PvField> {
            Some(self.seed.clone())
        }
        async fn put_value(&self, _name: &str, _v: PvField) -> Result<(), OpError> {
            Ok(())
        }
        async fn is_writable(&self, _name: &str) -> bool {
            false
        }
        async fn subscribe(&self, _name: &str) -> Option<MonitorStream<PvField>> {
            None
        }
        async fn subscribe_raw(
            &self,
            _name: &str,
        ) -> Option<MonitorStream<crate::server_native::RawMonitorEvent>> {
            self.raw_rx.lock().unwrap().take()
        }
    }

    /// The raw seed honors the per-event ACL recheck. An ACL reload that
    /// revokes READ during the idle window must suppress the raw seed
    /// (MONITOR FINISH, no seed DATA), symmetric with the decoded path where
    /// the seed is pending[0] and is rechecked on pop. Pre-fix the raw seed
    /// was emitted via `seed_cooked.take()` without the recheck, so a client
    /// that lost read access still received it.
    #[epics_macros_rs::epics_test]
    async fn pvx61_raw_seed_rechecks_acl_on_reload() {
        use epics_base_rs::server::access_security::{AccessGate, AsgAslResolver, parse_acf};
        let order = ByteOrder::Little;
        let (sid, ioid) = (12u32, 711u32);
        let intro = three_field_intro();

        // Permissive at subscribe: the anonymous peer may READ.
        let permissive = parse_acf("ASG(DEFAULT) {\n    RULE(1, READ)\n}\n").expect("acf");
        let cell = epics_base_rs::server::access_security::new_acf_cell(Some(permissive));
        let version = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let resolver: AsgAslResolver =
            std::sync::Arc::new(|_pv| Box::pin(async { ("DEFAULT".to_string(), 0u8) }));
        let gate = AccessGate::required_with_version(cell.clone(), resolver, version.clone());

        let (_raw_tx, raw_rx) = mpsc::channel::<crate::server_native::RawMonitorEvent>(64);
        let source: DynSource = Arc::new(FlipRawSeedSource {
            intro: intro.clone(),
            seed: three_field_value(0, 0, 0),
            raw_rx: std::sync::Mutex::new(Some(raw_rx.into())),
            gate,
        });
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        pvx61_drive(
            &pvx61_init_frame(sid, ioid, 8, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("MONITOR INIT ok");
        let _ = rx.recv().await.expect("INIT introspection reply");
        // Let the raw subscriber capture the seed under the permissive gate.
        epics_base_rs::runtime::task::sleep(Duration::from_millis(150)).await;

        // ACL reload during the idle window: READ is revoked, version bumped.
        let deny = parse_acf(
            "UAG(ops) { alice }\n\
             ASG(DEFAULT) {\n\
             \x20   RULE(0, READ) { UAG(ops) }\n\
             }\n",
        )
        .expect("acf");
        cell.store(Some(std::sync::Arc::new(deny)));
        version.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;

        // START: the raw seed emit rechecks ACL, sees the deny, and emits
        // FINISH instead of the seed DATA frame.
        pvx61_drive(
            &pvx61_ctrl_frame(sid, ioid, 0x44, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
        )
        .await
        .expect("MONITOR START ok");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;

        let mut data = Vec::new();
        let mut saw_finish = false;
        while let Ok(buf) = rx.try_recv() {
            if let Some(a) = pvx61_decode_data_a(&buf, &intro, order) {
                data.push(a);
            } else if pvx61_is_finish(&buf, order) {
                saw_finish = true;
            }
        }
        assert!(
            data.is_empty(),
            "the raw seed must be suppressed once READ is revoked mid-window; \
             got seed DATA {data:?}"
        );
        assert!(
            saw_finish,
            "a revoked raw seed must emit MONITOR FINISH, not the seed frame"
        );
    }

    /// A source whose `Required` ACL denies READ to the connecting
    /// (anonymous) peer. `subscribe` returns a live receiver, so the only
    /// thing blocking the subscription is the gate — proving the deny is
    /// ACL-driven, not an absent stream.
    #[cfg(test)]
    struct DenyReadSource {
        gate: epics_base_rs::server::access_security::AccessGate,
    }

    #[cfg(test)]
    impl DenyReadSource {
        fn new() -> Self {
            use epics_base_rs::server::access_security::{AsgAslResolver, parse_acf};
            // READ is granted only to UAG(ops)={alice}; the anonymous peer
            // matches no rule ⇒ NoAccess (default deny).
            let acf = parse_acf(
                "UAG(ops) { alice }\n\
                 ASG(DEFAULT) {\n\
                 \x20   RULE(0, READ) { UAG(ops) }\n\
                 }\n",
            )
            .expect("acf parse");
            let cell = epics_base_rs::server::access_security::new_acf_cell(Some(acf));
            let resolver: AsgAslResolver =
                std::sync::Arc::new(|_pv| Box::pin(async { ("DEFAULT".to_string(), 0u8) }));
            Self {
                gate: epics_base_rs::server::access_security::AccessGate::required(cell, resolver),
            }
        }
    }

    #[cfg(test)]
    impl crate::server_native::source::ChannelSource for DenyReadSource {
        fn access(&self) -> &epics_base_rs::server::access_security::AccessGate {
            &self.gate
        }
        async fn list_pvs(&self) -> Vec<String> {
            vec!["dut".into()]
        }
        async fn has_pv(&self, n: &str) -> bool {
            n == "dut"
        }
        async fn get_introspection(&self, _: &str) -> Option<FieldDesc> {
            Some(three_field_intro())
        }
        async fn get_value(&self, _: &str) -> Option<PvField> {
            Some(three_field_value(0, 0, 0))
        }
        async fn put_value(&self, _: &str, _v: PvField) -> Result<(), OpError> {
            Ok(())
        }
        async fn is_writable(&self, _: &str) -> bool {
            false
        }
        async fn subscribe(&self, _: &str) -> Option<MonitorStream<PvField>> {
            let (_tx, rx) = mpsc::channel(4);
            Some(rx.into())
        }
    }

    /// ACL denial surfaces at INIT (an approved semantic shift, documented
    /// in the commit).
    /// pvxs registers the upstream at INIT (`onSubscribe`), so an ACL
    /// deny now fails the subscribe at INIT: the subscriber task ends
    /// immediately (its terminal `MonitorFinished` fires) and no DATA
    /// frame ever flows — not even after a later START. Pre-fix the
    /// subscribe (and therefore the deny) was deferred to START.
    ///
    /// The complementary case — an ACL that flips to DENY *mid-window*,
    /// forcing the per-event `revalidate_read` recheck to emit MONITOR
    /// FINISH — is exercised end-to-end (over the wire, through
    /// `run_pva_server`) by
    /// `tests/parity/test_monitor_reload_deny_composite.rs`; this change
    /// preserved that per-event recheck arm unchanged, so that test
    /// remains its coverage.
    #[epics_macros_rs::epics_test]
    async fn pvx61_acl_deny_surfaces_at_init() {
        let order = ByteOrder::Little;
        let (sid, ioid) = (10u32, 709u32);
        let intro = three_field_intro();
        let source: DynSource = Arc::new(DenyReadSource::new());
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);
        let (mon_fin_tx, mut mon_fin_rx) = mpsc::unbounded_channel::<MonitorFinished>();

        pvx61_drive(
            &pvx61_init_frame(sid, ioid, 8, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &mon_fin_tx,
        )
        .await
        .expect("MONITOR INIT ok");
        let _ = rx.recv().await.expect("INIT introspection reply");

        // The subscribe is denied AT INIT: the subscriber task returns
        // immediately, firing its terminal MonitorFinished.
        let fin = epics_base_rs::runtime::task::timeout(Duration::from_secs(2), mon_fin_rx.recv())
            .await
            .expect("ACL deny tears the subscriber down at INIT within 2s")
            .expect("MonitorFinished");
        assert_eq!(fin.ioid, ioid, "the denied op tore down at INIT");

        // Even a subsequent START yields no DATA frame — the subscriber
        // is already gone.
        pvx61_drive(
            &pvx61_ctrl_frame(sid, ioid, 0x44, order),
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
            &mon_fin_tx,
        )
        .await
        .expect("MONITOR START ok");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;
        let seq = pvx61_drain_data_a(&mut rx, &intro, order);
        assert!(
            seq.is_empty(),
            "an ACL-denied monitor emits no DATA frame; got {seq:?}"
        );
    }

    /// A pipelined MONITOR INIT that omits the 0x80 initial-nack
    /// rider must seed the credit window to 0, NOT to the negotiated
    /// `queueSize`. pvxs initialises `nack = 0` and assigns
    /// `op->window = nack` (`servermon.cpp:483,519`); `queueSize` feeds
    /// `op->limit`/queue depth only. So a conforming-but-rider-less
    /// pipeline monitor must STALL — zero DATA frames flow until the
    /// client grants credit with a MONITOR_ACK. The pre-fix
    /// `unwrap_or(queue_size)` shipped up to `queueSize` unacked frames,
    /// re-opening the non-zero-initial-window hazard pvxs warns against.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pipeline_absent_nack_stalls_until_ack() {
        use crate::server_native::SharedSource;
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::shared_pv::SharedPV;

        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 701;

        let intro = three_field_intro();
        let pv = SharedPV::new();
        pv.open(intro.clone(), three_field_value(0, 0, 0)).unwrap();
        let pusher = pv.clone();

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
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // MONITOR INIT with pipeline=true, queueSize=2, but subcmd 0x08:
        // the 0x80 bit is CLEAR, so NO initial-nack rider follows the
        // pvRequest. The credit window must seed to 0.
        let req_val = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(2)),
        );
        let req_desc = req_val.descriptor();
        let mut init_payload = Vec::new();
        init_payload.put_u32(sid, order);
        init_payload.put_u32(ioid, order);
        init_payload.put_u8(0x08);
        crate::pvdata::encode::encode_type_desc(&req_desc, order, &mut init_payload);
        crate::pvdata::encode::encode_pv_field(&req_val, &req_desc, order, &mut init_payload);
        let init_frame = synth_frame(Command::Monitor, order, init_payload);
        handle_op(
            &init_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("MONITOR INIT ok");
        let _ = rx.recv().await.expect("INIT reply");

        // MONITOR START (subcmd 0x44 = start | process).
        let mut start_payload = Vec::new();
        start_payload.put_u32(sid, order);
        start_payload.put_u32(ioid, order);
        start_payload.put_u8(0x44);
        let start_frame = synth_frame(Command::Monitor, order, start_payload);
        handle_op(
            &start_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("MONITOR START ok");

        // Let the task subscribe; with a zero window the initial snapshot
        // cannot be sent. Push updates with NO ACK — all must stall.
        tokio::time::sleep(Duration::from_millis(300)).await;
        for i in 1..=8 {
            pusher.try_post(three_field_value(i, 0, 0));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;

        let mut data_frames = 0usize;
        while rx.try_recv().is_ok() {
            data_frames += 1;
        }
        assert_eq!(
            data_frames, 0,
            "absent initial-nack rider must seed the window to 0, so a \
             pipeline monitor with no ACK sends ZERO DATA frames; got \
             {data_frames} — the window was pre-credited to queueSize"
        );
    }

    /// A NON-pipeline monitor that requests `record[queueSize=2]` must
    /// have that depth honoured as its server-side squash threshold, not
    /// silently discarded in favour of `PvaServerConfig::monitor_queue_depth`
    /// (default 64). pvxs assigns `op->limit = qSize` for any valid
    /// queueSize OUTSIDE the `if(op->pipeline)` block (servermon.cpp:533-543)
    /// and the squash compares `queue.size() < limit` (servermon.cpp:271-287).
    /// Here we drive a plain (pipeline=false) MONITOR INIT and assert the
    /// negotiated `queueSize` lands on `OpState.monitor_options.queue_size`
    /// — the exact field the START path reads to set the squash threshold.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_pipeline_queue_size_drives_server_squash_threshold() {
        use crate::server_native::SharedSource;
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::shared_pv::SharedPV;

        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 909;

        let intro = three_field_intro();
        let pv = SharedPV::new();
        pv.open(intro.clone(), three_field_value(0, 0, 0)).unwrap();
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
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        assert_ne!(
            config.monitor_queue_depth, 2,
            "test premise: the requested depth must differ from the server default"
        );
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // MONITOR INIT, pipeline OFF, queueSize=2.
        let req_val = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(false)),
            PvField::Scalar(ScalarValue::Int(2)),
        );
        let req_desc = req_val.descriptor();
        let mut init_payload = Vec::new();
        init_payload.put_u32(sid, order);
        init_payload.put_u32(ioid, order);
        init_payload.put_u8(0x08);
        crate::pvdata::encode::encode_type_desc(&req_desc, order, &mut init_payload);
        crate::pvdata::encode::encode_pv_field(&req_val, &req_desc, order, &mut init_payload);
        let init_frame = synth_frame(Command::Monitor, order, init_payload);
        handle_op(
            &init_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("non-pipeline MONITOR INIT ok");
        let _ = rx.recv().await.expect("INIT reply");

        let op = channels
            .get(&sid)
            .and_then(|c| c.ops.get(&ioid))
            .expect("op present after INIT");
        // pipeline flow control must stay OFF...
        assert!(
            op.monitor_window.is_none(),
            "pipeline=false must not negotiate a credit window"
        );
        // ...yet the requested queueSize is preserved as the per-op squash
        // depth the START path consumes (was discarded before the fix).
        assert_eq!(
            op.monitor_options.queue_size, 2,
            "non-pipeline queueSize=2 must be the per-op squash threshold, \
             not the server default {}",
            config.monitor_queue_depth
        );
    }

    /// Regression (PVA parity): only a real START frame (`0x44`) may
    /// move a MONITOR from Idle to Executing. pvxs gates START/STOP on
    /// `subcmd & 0x04` with `start = subcmd & 0x40` (`servermon.cpp:671-683`)
    /// and treats an ACK (`0x80`) as window-refill only (`:643-669`); a
    /// plain `0x00` carries no stream-control bit and does nothing. The
    /// pre-fix `is_start_or_ack = 0x40 | ack | 0x00` union let an
    /// ACK-only or `0x00` frame spawn the subscriber task and fire the
    /// initial `onStart(true)` before the client ever sent START.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn monitor_ack_only_or_zero_subcmd_does_not_start_until_real_start() {
        use crate::server_native::SharedSource;
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::shared_pv::SharedPV;

        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 720;

        let intro = three_field_intro();
        let pv = SharedPV::new();
        pv.open(intro.clone(), three_field_value(0, 0, 0)).unwrap();
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
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // MONITOR INIT (pipeline, queueSize=4 so an ACK is well-formed).
        let req_val = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(4)),
        );
        let req_desc = req_val.descriptor();
        let mut init_payload = Vec::new();
        init_payload.put_u32(sid, order);
        init_payload.put_u32(ioid, order);
        init_payload.put_u8(0x08);
        crate::pvdata::encode::encode_type_desc(&req_desc, order, &mut init_payload);
        crate::pvdata::encode::encode_pv_field(&req_val, &req_desc, order, &mut init_payload);
        let init_frame = synth_frame(Command::Monitor, order, init_payload);
        handle_op(
            &init_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("MONITOR INIT ok");
        let _ = rx.recv().await.expect("INIT reply");

        // The subscriber task is spawned at INIT (so `monitor_started`
        // is now set from INIT), so "is the monitor delivering?" is the
        // Executing edge owned by `MonitorStartControl`, not `monitor_started`.
        // A real START (0x44) flips it; a plain 0x00 or an ACK-only frame must
        // not.
        let started = |chs: &HashMap<u32, ChannelState>| -> bool {
            chs.get(&sid)
                .and_then(|c| c.ops.get(&ioid))
                .and_then(|o| o.monitor_start_ctl.as_ref())
                .map(|ctl| ctl.is_executing())
                .expect("op + start-control present after INIT")
        };
        assert!(
            !started(&channels),
            "monitor must be idle (not executing) right after INIT"
        );

        // Sync builder for a data-phase control frame with a given subcmd
        // (and optional trailing pipeline ack-count).
        let control_frame = |subcmd: u8, ack_count: Option<u32>| -> Frame {
            let mut payload = Vec::new();
            payload.put_u32(sid, order);
            payload.put_u32(ioid, order);
            payload.put_u8(subcmd);
            if let Some(c) = ack_count {
                payload.put_u32(c, order);
            }
            synth_frame(Command::Monitor, order, payload)
        };

        // Plain 0x00: no stream-control bit → monitor stays idle.
        handle_op(
            &control_frame(0x00, None),
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("0x00 control ok");
        assert!(
            !started(&channels),
            "a plain 0x00 control frame must not start the monitor"
        );

        // ACK-only 0x80 (+count): refills the window, never starts.
        handle_op(
            &control_frame(0x80, Some(4)),
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("ACK control ok");
        assert!(
            !started(&channels),
            "an ACK-only frame must refill credit only, never start the monitor"
        );

        // Real START 0x44 (0x04 START/STOP | 0x40 start) → Executing.
        handle_op(
            &control_frame(0x44, None),
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("START control ok");
        assert!(
            started(&channels),
            "a real START (0x44) must move the monitor to Executing"
        );
    }

    /// Build a `channels` map with a single live (started) MONITOR op
    /// whose pipeline window starts at 0, for the ACK-payload tests.
    fn ack_test_channels(
        sid: u32,
        ioid: u32,
        window: std::sync::Arc<std::sync::atomic::AtomicU32>,
        source: DynSource,
    ) -> HashMap<u32, ChannelState> {
        let mut ops = HashMap::new();
        ops.insert(
            ioid,
            OpState {
                intro: std::sync::Arc::new(FieldDesc::Variant),
                kind: OpKind::Monitor,
                monitor_started: true,
                monitor_abort: None,
                mask: BitSet::new(),
                put_mask: None,
                monitor_window: Some(window),
                monitor_window_notify: Some(Arc::new(tokio::sync::Notify::new())),
                monitor_paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                monitor_resume: Arc::new(tokio::sync::Notify::new()),
                monitor_wm: None,
                monitor_wm_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
                monitor_op_id: next_op_id(),
                monitor_filters: Arc::new(
                    epics_base_rs::server::database::filters::FilterChain::new(),
                ),
                pv_request: None,
                monitor_options: crate::server_native::source::MonitorOptions::default(),
                data_task_abort: None,
                monitor_start_ctl: None,
                exec_state: ExecState::Idle,
                last_request: false,
            },
        );
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: None,
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops,
            },
        );
        channels
    }

    /// pvxs `servermon.cpp:599-608`: a data-phase MONITOR ACK
    /// (`subcmd & 0x80`) whose u32 ack-count payload is missing/truncated
    /// is `!M.good()` → `bev.reset()` (connection-fatal). The previous
    /// `unwrap_or(4)` fabricated 4 credits from a malformed ACK on a live
    /// monitor, silently corrupting the flow-control window.
    #[epics_macros_rs::epics_test]
    async fn monitor_ack_truncated_payload_is_fatal() {
        use std::sync::atomic::AtomicU32;
        let order = ByteOrder::Little;
        let sid: u32 = 3;
        let ioid: u32 = 88;

        let window = Arc::new(AtomicU32::new(0));
        let source: DynSource = Arc::new(crate::server_native::SharedSource::new());
        let mut channels = ack_test_channels(sid, ioid, window.clone(), source.clone());
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let config = crate::server_native::runtime::PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // ACK frame (subcmd 0x80) with NO u32 ack-count payload.
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x80);
        let frame = synth_frame(Command::Monitor, order, payload);
        let err = handle_op(
            &frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect_err("truncated MONITOR ACK must be connection-fatal");
        assert!(
            matches!(err, PvaError::Decode(_)),
            "expected a Decode (connection-reset) error, got {err:?}"
        );
        assert_eq!(
            window.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a malformed ACK must NOT fabricate credits"
        );
    }

    /// A well-formed MONITOR ACK refills the pipeline window by exactly
    /// the decoded count — no fabricated default.
    #[epics_macros_rs::epics_test]
    async fn monitor_ack_well_formed_refills_window_by_count() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let order = ByteOrder::Little;
        let sid: u32 = 3;
        let ioid: u32 = 88;

        let window = Arc::new(AtomicU32::new(0));
        let source: DynSource = Arc::new(crate::server_native::SharedSource::new());
        let mut channels = ack_test_channels(sid, ioid, window.clone(), source.clone());

        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let config = crate::server_native::runtime::PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x80);
        payload.put_u32(3, order); // ack-count
        let frame = synth_frame(Command::Monitor, order, payload);
        handle_op(
            &frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("well-formed MONITOR ACK ok");
        assert_eq!(
            window.load(Ordering::Relaxed),
            3,
            "window must refill by exactly the decoded ack-count"
        );
    }

    /// A MONITOR ACK whose count would push the window past `u32::MAX`
    /// SATURATES the stored credit instead of wrapping it. A raw
    /// `fetch_add` would wrap to a tiny value, leaving `acquire`'s view
    /// of the credit far below what the watermark logic computed in
    /// `usize` — the divergence pvxs avoids with a `size_t` window.
    #[epics_macros_rs::epics_test]
    async fn monitor_ack_refill_saturates_instead_of_wrapping() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let order = ByteOrder::Little;
        let sid: u32 = 3;
        let ioid: u32 = 88;

        // Window already near the cap; a large ACK would wrap a raw u32.
        let window = Arc::new(AtomicU32::new(u32::MAX - 1));
        let source: DynSource = Arc::new(crate::server_native::SharedSource::new());
        let mut channels = ack_test_channels(sid, ioid, window.clone(), source.clone());

        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let config = crate::server_native::runtime::PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x80);
        payload.put_u32(100, order); // would overflow (MAX-1) + 100
        let frame = synth_frame(Command::Monitor, order, payload);
        handle_op(
            &frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("well-formed MONITOR ACK ok");
        assert_eq!(
            window.load(Ordering::Relaxed),
            u32::MAX,
            "refill must saturate at u32::MAX, not wrap to a tiny value"
        );
    }

    /// Recorder source for the MONITOR ACK validate-order regression.
    /// Pushes every
    /// `notify_monitor_start` and `notify_watermark` edge into one ordered
    /// log so a test can assert (a) NO edge fires when a malformed combined
    /// frame is rejected, and (b) ACK refill precedes START for a well-formed
    /// combined frame.
    struct AckOrderSource {
        intro: FieldDesc,
        value: PvField,
        log: Arc<parking_lot::Mutex<Vec<String>>>,
    }
    impl crate::server_native::source::ChannelSource for AckOrderSource {
        async fn list_pvs(&self) -> Vec<String> {
            vec!["dut".into()]
        }
        async fn has_pv(&self, n: &str) -> bool {
            n == "dut"
        }
        async fn get_introspection(&self, _n: &str) -> Option<FieldDesc> {
            Some(self.intro.clone())
        }
        async fn get_value(&self, _n: &str) -> Option<PvField> {
            Some(self.value.clone())
        }
        async fn put_value(&self, _n: &str, _v: PvField) -> Result<(), OpError> {
            Ok(())
        }
        async fn is_writable(&self, _n: &str) -> bool {
            false
        }
        async fn subscribe(&self, _n: &str) -> Option<MonitorStream<PvField>> {
            let (_tx, rx) = mpsc::channel(1);
            Some(rx.into())
        }
        fn notify_monitor_start(
            &self,
            _name: &str,
            _ctx: &crate::server_native::source::ChannelContext,
            start: bool,
        ) {
            self.log.lock().push(format!("start:{start}"));
        }
        fn notify_watermark(
            &self,
            _name: &str,
            _ctx: &crate::server_native::source::ChannelContext,
            ev: crate::server_native::source::WatermarkEvent,
        ) {
            self.log.lock().push(format!("watermark:{:?}", ev.kind));
        }
    }

    /// Build a single started MONITOR op for the ACK validate-order tests.
    /// `paused` is the caller-held `monitor_paused` handle (so it can assert
    /// on it after the call); `executing` fires the initial Idle->Executing
    /// edge on `monitor_start_ctl` (recording "start:true" — callers clear the
    /// log after build when they need a clean slate); `wm`, when set, also
    /// seeds the watermark sequence to the "below high" (drained) parity so an
    /// ACK refill can cross HIGH.
    fn ack_order_channels(
        ids: (u32, u32),
        paused: Arc<std::sync::atomic::AtomicBool>,
        executing: bool,
        window: Arc<std::sync::atomic::AtomicU32>,
        wm: Option<(usize, usize)>,
        src: &DynSource,
        intro: &FieldDesc,
    ) -> HashMap<u32, ChannelState> {
        let (sid, ioid) = ids;
        let mut op = non_monitor_op_state(
            std::sync::Arc::new(intro.clone()),
            OpKind::Monitor,
            BitSet::all_set(intro.total_bits()),
        );
        op.monitor_started = true;
        op.monitor_window = Some(window);
        op.monitor_window_notify = Some(Arc::new(tokio::sync::Notify::new()));
        op.monitor_paused = paused;
        op.monitor_resume = Arc::new(tokio::sync::Notify::new());
        op.monitor_wm = wm;
        // even parity = "below high" (a drained window awaiting refill), odd =
        // "above" (full window). `cross_watermark` fires HIGH only from the
        // below state, so a watermark test seeds 0; without a watermark the
        // seed is irrelevant (matches the production full-window seed of 1).
        op.monitor_wm_seq = Arc::new(std::sync::atomic::AtomicU64::new(if wm.is_some() {
            0
        } else {
            1
        }));
        let (exec_tx, _exec_rx) = tokio::sync::watch::channel(false);
        let ctl = Arc::new(MonitorStartControl::new(
            src.clone(),
            "dut".into(),
            bfr12_anon_ctx(),
            exec_tx,
        ));
        if executing {
            ctl.set(true); // Idle->Executing edge (records "start:true")
        }
        op.monitor_start_ctl = Some(ctl);
        let mut ops = HashMap::new();
        ops.insert(ioid, op);
        let mut channels = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: src.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops,
            },
        );
        channels
    }

    /// A truncated `0xC4` (ACK|START) frame with no ACK `u32` must be a
    /// connection-fatal Decode error with NO START side effect: `monitor_paused`
    /// stays paused and `notify_monitor_start` never fires. pvxs
    /// (servermon.cpp:599-608) reads/validates the ACK count before any op
    /// lookup or `onStart`. Pre-fix the START block ran first, clearing the
    /// pause and firing `notify_monitor_start(true)` before the decode error.
    #[epics_macros_rs::epics_test]
    async fn monitor_combined_ack_start_truncated_no_side_effect() {
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        let order = ByteOrder::Little;
        let (sid, ioid) = (3u32, 88u32);
        let intro = three_field_intro();
        let log = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
        let src: DynSource = Arc::new(AckOrderSource {
            intro: intro.clone(),
            value: three_field_value(0, 0, 0),
            log: log.clone(),
        });
        let window = Arc::new(AtomicU32::new(0));
        // paused=true so a real START would resume (observable); ctl Idle
        // (executing=false) so a real START would fire notify_monitor_start.
        let paused = Arc::new(AtomicBool::new(true));
        let mut channels = ack_order_channels(
            (sid, ioid),
            paused.clone(),
            false,
            window.clone(),
            None,
            &src,
            &intro,
        );
        log.lock().clear();

        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let config = crate::server_native::runtime::PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // subcmd 0xC4 = ACK(0x80)|START(0x04)|start(0x40), NO ack u32 payload.
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0xC4);
        let frame = synth_frame(Command::Monitor, order, payload);
        let err = handle_op(
            &frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect_err("truncated ACK|START must be connection-fatal");
        assert!(
            matches!(err, PvaError::Decode(_)),
            "expected a Decode (connection-reset) error, got {err:?}"
        );
        assert!(
            paused.load(Ordering::Relaxed),
            "a rejected ACK|START must NOT clear monitor_paused before the decode error"
        );
        assert!(
            log.lock().is_empty(),
            "a rejected ACK|START must fire no start/watermark callback, got {:?}",
            log.lock()
        );
    }

    /// A truncated `0x84` (ACK|STOP) frame with no ACK `u32` must be a
    /// connection-fatal Decode error with NO STOP side effect: `monitor_paused`
    /// stays cleared and `notify_monitor_start(false)` never fires. Pre-fix the
    /// STOP block stored `monitor_paused=true` and fired the stop edge before
    /// the decode error.
    #[epics_macros_rs::epics_test]
    async fn monitor_combined_ack_stop_truncated_no_side_effect() {
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        let order = ByteOrder::Little;
        let (sid, ioid) = (3u32, 88u32);
        let intro = three_field_intro();
        let log = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
        let src: DynSource = Arc::new(AckOrderSource {
            intro: intro.clone(),
            value: three_field_value(0, 0, 0),
            log: log.clone(),
        });
        let window = Arc::new(AtomicU32::new(0));
        // paused=false and ctl Executing (executing=true) so a real STOP would
        // pause and fire notify_monitor_start(false).
        let paused = Arc::new(AtomicBool::new(false));
        let mut channels = ack_order_channels(
            (sid, ioid),
            paused.clone(),
            true,
            window.clone(),
            None,
            &src,
            &intro,
        );
        log.lock().clear(); // drop the build-time "start:true" edge

        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let config = crate::server_native::runtime::PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // subcmd 0x84 = ACK(0x80)|STOP(0x04, no start bit), NO ack u32 payload.
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x84);
        let frame = synth_frame(Command::Monitor, order, payload);
        let err = handle_op(
            &frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect_err("truncated ACK|STOP must be connection-fatal");
        assert!(
            matches!(err, PvaError::Decode(_)),
            "expected a Decode (connection-reset) error, got {err:?}"
        );
        assert!(
            !paused.load(Ordering::Relaxed),
            "a rejected ACK|STOP must NOT set monitor_paused before the decode error"
        );
        assert!(
            log.lock().is_empty(),
            "a rejected ACK|STOP must fire no stop/watermark callback, got {:?}",
            log.lock()
        );
    }

    /// A well-formed `0xC4` (ACK|START) refills the window AND resumes, and
    /// pvxs (servermon.cpp:643-689) applies ACK refill THEN START, so the
    /// `onHighMark` (Resume watermark) precedes `onStart`. Pre-fix the START
    /// block ran first, reversing the order to [start, watermark].
    #[epics_macros_rs::epics_test]
    async fn monitor_combined_ack_start_wellformed_acks_before_start() {
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        let order = ByteOrder::Little;
        let (sid, ioid) = (3u32, 88u32);
        let intro = three_field_intro();
        let log = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
        let src: DynSource = Arc::new(AckOrderSource {
            intro: intro.clone(),
            value: three_field_value(0, 0, 0),
            log: log.clone(),
        });
        // Drained window (0) with high=0 so any refill crosses HIGH; paused so
        // START is a real resume; ctl Idle so the resume fires onStart(true).
        let window = Arc::new(AtomicU32::new(0));
        let paused = Arc::new(AtomicBool::new(true));
        let mut channels = ack_order_channels(
            (sid, ioid),
            paused.clone(),
            false,
            window.clone(),
            Some((0, 0)),
            &src,
            &intro,
        );
        log.lock().clear();

        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let config = crate::server_native::runtime::PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // subcmd 0xC4 with a 3-credit ACK count.
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0xC4);
        payload.put_u32(3, order);
        let frame = synth_frame(Command::Monitor, order, payload);
        handle_op(
            &frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("well-formed ACK|START ok");
        assert_eq!(
            window.load(Ordering::Relaxed),
            3,
            "ACK must refill the window by the decoded count"
        );
        assert!(
            !paused.load(Ordering::Relaxed),
            "a well-formed START must resume the monitor"
        );
        assert_eq!(
            *log.lock(),
            vec!["watermark:Resume".to_string(), "start:true".to_string()],
            "pvxs order: ACK refill (onHighMark) precedes START (onStart)"
        );
    }

    fn synth_frame(command: Command, order: ByteOrder, payload: Vec<u8>) -> Frame {
        let header = PvaHeader::application(false, order, command.code(), payload.len() as u32);
        Frame { header, payload }
    }

    /// Build the body of a SEARCH frame (everything after the 8-byte
    /// header) with an arbitrary advertised protocol list. The codec's
    /// `build_search_batch` hardcodes the protocol to `"tcp"`, so a
    /// `["tls"]`-protocol SEARCH (the case that exercises the TCP-circuit
    /// protocol gate) has to be hand-assembled here. Layout mirrors
    /// `parse_search_request` (udp.rs): seq, flags, 3 reserved, 16-byte
    /// reply addr, reply port, protocol-count + entries, query-count +
    /// (cid, name) entries.
    fn build_search_body(
        order: ByteOrder,
        seq: u32,
        flags: u8,
        protocols: &[&str],
        queries: &[(u32, &str)],
    ) -> Vec<u8> {
        let mut body = Vec::new();
        body.put_u32(seq, order);
        body.put_u8(flags);
        body.extend_from_slice(&[0u8; 3]); // reserved
        body.extend_from_slice(&[0u8; 16]); // reply addr (unspecified)
        body.put_u16(0, order); // reply port (unused by TCP circuit)
        crate::proto::encode_size_into(protocols.len() as u32, order, &mut body);
        for p in protocols {
            crate::proto::encode_string_into(p, order, &mut body);
        }
        body.put_u16(queries.len() as u16, order);
        for (cid, name) in queries {
            body.put_u32(*cid, order);
            crate::proto::encode_string_into(name, order, &mut body);
        }
        body
    }

    /// A SEARCH
    /// arriving on an established **plaintext TCP** circuit that advertises
    /// only `["tls"]` must still match a hosted PV. pvxs `handle_SEARCH`
    /// parses the protocol strings into `foundtcp` but never consults it
    /// before calling every source's `onSearch` (serverchan.cpp:184-244):
    /// the transport was already negotiated when the circuit opened, so the
    /// payload's protocol list does not re-gate matches on that circuit.
    /// (The byte-exact protocol gate is kept on the UDP responders — see
    /// `udp::search_matched_cids_gates_on_protocol` — where a broadcast
    /// SEARCH must not pull `found=1` from a server that does not speak the
    /// requested transport.)
    ///
    /// Both sub-cases are asserted: without `MustReply` the response still
    /// carries the found CID (because the match is real, not a forced
    /// probe-reply), and with `MustReply` it likewise carries the CID.
    #[epics_macros_rs::epics_test]
    async fn tcp_search_does_not_gate_on_advertised_protocol() {
        use crate::decode::{decode_search_response, try_parse_frame};
        use crate::server_native::{SharedPV, SharedSource};

        let order = ByteOrder::Little;
        let shared = SharedSource::new();
        shared.add("MY:PV", SharedPV::new());
        let source: DynSource = Arc::new(shared);
        // Plaintext TCP server: tls = None, so `protocol` == "tcp", while
        // the SEARCH below advertises only "tls".
        let config = PvaServerConfig::default();
        assert!(
            config.tls.is_none(),
            "this regression targets the tcp circuit"
        );
        let peer: SocketAddr = "127.0.0.1:34567".parse().unwrap();

        // --- (a) no MustReply: the real match alone must produce a reply. ---
        let body = build_search_body(order, 7, 0x00, &["tls"], &[(42, "MY:PV")]);
        let frame = synth_frame(Command::Search, order, body);
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(8);
        handle_tcp_search(&source, &frame, &tx, &config, peer)
            .await
            .expect("handle_tcp_search must succeed");
        let raw = rx
            .try_recv()
            .expect("a matching tcp SEARCH must emit a SEARCH_RESPONSE even with no MustReply");
        let (resp_frame, _) = try_parse_frame(&raw)
            .expect("response must parse")
            .expect("response must be a complete frame");
        let resp = decode_search_response(&resp_frame).expect("decode SEARCH_RESPONSE");
        assert!(
            resp.found,
            "found byte must be set: the tcp circuit matched MY:PV"
        );
        assert_eq!(resp.cids, vec![42], "the matched CID must be echoed back");
        assert_eq!(resp.seq, 7, "the SEARCH seq must be echoed");

        // --- (b) MustReply: the same match must still carry the CID. ---
        let body = build_search_body(order, 9, 0x01, &["tls"], &[(43, "MY:PV")]);
        let frame = synth_frame(Command::Search, order, body);
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(8);
        handle_tcp_search(&source, &frame, &tx, &config, peer)
            .await
            .expect("handle_tcp_search must succeed");
        let raw = rx
            .try_recv()
            .expect("MustReply must emit a SEARCH_RESPONSE");
        let (resp_frame, _) = try_parse_frame(&raw)
            .expect("response must parse")
            .expect("response must be a complete frame");
        let resp = decode_search_response(&resp_frame).expect("decode SEARCH_RESPONSE");
        assert!(resp.found, "MustReply match must set found");
        assert_eq!(
            resp.cids,
            vec![43],
            "MustReply must still carry the matched CID"
        );
        assert_eq!(resp.seq, 9);
    }

    /// Companion to the no-gate case above: a TCP-circuit SEARCH for a name
    /// this server does NOT host, with no `MustReply`, emits nothing. This
    /// pins that dropping the protocol gate did not turn the TCP handler
    /// into a reply-to-everything path — a genuine name miss is still
    /// silent (pvxs serverchan.cpp:240-249 only replies on a match or
    /// MustReply).
    #[epics_macros_rs::epics_test]
    async fn tcp_search_unknown_name_without_mustreply_is_silent() {
        use crate::server_native::{SharedPV, SharedSource};

        let order = ByteOrder::Little;
        let shared = SharedSource::new();
        shared.add("MY:PV", SharedPV::new());
        let source: DynSource = Arc::new(shared);
        let config = PvaServerConfig::default();
        let peer: SocketAddr = "127.0.0.1:34567".parse().unwrap();

        let body = build_search_body(order, 11, 0x00, &["tls"], &[(50, "OTHER:PV")]);
        let frame = synth_frame(Command::Search, order, body);
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(8);
        handle_tcp_search(&source, &frame, &tx, &config, peer)
            .await
            .expect("handle_tcp_search must succeed");
        assert!(
            rx.try_recv().is_err(),
            "an unmatched name with no MustReply must not emit a SEARCH_RESPONSE"
        );
    }

    #[test]
    fn handle_message_does_not_panic_on_well_formed_input() {
        // Wire layout: ioid (u32) + messageType (u8) + message (string).
        // We can't easily inspect tracing output here, so the assertion is
        // simply that the handler tolerates each severity level without
        // panicking and consumes the cursor cleanly. With an empty op
        // map the IOID is unknown, so each MESSAGE is dropped at debug
        // (pvxs serverconn.cpp:338-342) — still Ok.
        let order = ByteOrder::Little;
        let peer = "127.0.0.1:5075".parse::<SocketAddr>().unwrap();
        let channels: HashMap<u32, ChannelState> = HashMap::new();
        for mtype in [0u8, 1, 2, 3, 9] {
            let mut payload = Vec::new();
            payload.put_u32(0xDEADBEEF, order); // ioid
            payload.put_u8(mtype);
            crate::proto::encode_string_into("hello from client", order, &mut payload);
            let frame = synth_frame(Command::Message, order, payload);
            // MESSAGE handler now returns PvaResult; well-formed
            // payload must succeed.
            handle_message(&frame, &channels, &peer).expect("well-formed MESSAGE");
        }

        // truncated MESSAGE is now a protocol-fatal decode
        // error (matches pvxs `serverconn.cpp:323-336` throw). The
        // server loop turns this into a connection reset.
        let frame_short = synth_frame(Command::Message, order, vec![0x01, 0x02]);
        let err =
            handle_message(&frame_short, &channels, &peer).expect_err("truncated MESSAGE must Err");
        assert!(
            matches!(err, PvaError::Decode(_)),
            "expected Decode error, got {err:?}"
        );
    }

    /// pvxs gates inbound MESSAGE on the connection-wide op map: a
    /// MESSAGE for an IOID no operation owns is dropped (debug only),
    /// while a MESSAGE for a live IOID is accepted and (in pvxs) tagged
    /// with the owning channel name. Both return Ok here; the
    /// regression pins that an unknown IOID does NOT error and a live
    /// IOID resolves its owning channel.
    #[test]
    fn handle_message_gates_on_live_ioid() {
        let order = ByteOrder::Little;
        let peer = "127.0.0.1:5075".parse::<SocketAddr>().unwrap();

        // Build a channel with a live op for ioid 7.
        let source: DynSource = Arc::new(crate::server_native::SharedSource::new());
        let mut ops = HashMap::new();
        ops.insert(
            7u32,
            non_monitor_op_state(
                std::sync::Arc::new(FieldDesc::Scalar(ScalarType::Int)),
                OpKind::Get,
                BitSet::new(),
            ),
        );
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            1,
            ChannelState {
                name: "dut:pv".into(),
                cid: 0,
                sid: 1,
                introspection: Some(std::sync::Arc::new(FieldDesc::Scalar(ScalarType::Int))),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops,
            },
        );

        let msg = |ioid: u32| -> PvaResult<()> {
            let mut payload = Vec::new();
            payload.put_u32(ioid, order);
            payload.put_u8(1); // warning
            crate::proto::encode_string_into("hi", order, &mut payload);
            let frame = synth_frame(Command::Message, order, payload);
            handle_message(&frame, &channels, &peer)
        };
        // Live IOID: accepted.
        msg(7).expect("MESSAGE on live IOID accepted");
        // Unknown IOID: dropped, not an error, no severity escalation.
        msg(999).expect("MESSAGE on unknown IOID dropped, not an error");
    }

    /// A data-phase (non-INIT) operation frame must resolve its channel
    /// through the connection-wide op owner, not the SID carried in the
    /// frame. pvxs GET/PUT/RPC EXEC looks the op up in `opByIOID` and acts
    /// on `op->chan`, ignoring the frame SID (serverget.cpp:421-423);
    /// MONITOR additionally resets the circuit when the frame SID does not
    /// own the IOID (servermon.cpp:610-635).
    #[test]
    fn data_phase_owner_sid_resolves_connection_wide() {
        let mk_channel = |sid: u32, ioid: u32| -> ChannelState {
            let source: DynSource = Arc::new(crate::server_native::SharedSource::new());
            let mut ops = HashMap::new();
            ops.insert(
                ioid,
                non_monitor_op_state(
                    std::sync::Arc::new(FieldDesc::Scalar(ScalarType::Int)),
                    OpKind::Get,
                    BitSet::new(),
                ),
            );
            ChannelState {
                name: format!("dut:pv{sid}"),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(FieldDesc::Scalar(ScalarType::Int))),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops,
            }
        };
        // Channel 1 owns IOID 7; channel 2 owns IOID 9.
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(1, mk_channel(1, 7));
        channels.insert(2, mk_channel(2, 9));

        // GPR-like (require_sid_match=false): a frame naming the WRONG SID
        // for a live IOID still resolves to the owning channel — the frame
        // SID is ignored, matching pvxs serverget.cpp:421-423.
        assert_eq!(
            data_phase_owner_sid(&channels, 7, 2, false).unwrap(),
            Some(1),
            "GPR data frame must dispatch on the IOID owner, not the frame SID"
        );
        // Correct SID also resolves to the owner.
        assert_eq!(
            data_phase_owner_sid(&channels, 7, 1, false).unwrap(),
            Some(1)
        );
        // MONITOR (require_sid_match=true): a frame SID that does not own
        // the IOID is connection-fatal (pvxs servermon.cpp:631-635).
        assert!(
            data_phase_owner_sid(&channels, 7, 2, true).is_err(),
            "MONITOR data frame with a mismatched SID must be connection-fatal"
        );
        // MONITOR with the matching SID resolves normally.
        assert_eq!(
            data_phase_owner_sid(&channels, 7, 1, true).unwrap(),
            Some(1)
        );
        // Unknown IOID → fall through to the caller's not-found path (the
        // DESTROY race), for both GPR and MONITOR.
        assert_eq!(
            data_phase_owner_sid(&channels, 999, 1, false).unwrap(),
            None
        );
        assert_eq!(data_phase_owner_sid(&channels, 999, 1, true).unwrap(), None);
    }

    /// pvxs maps inbound CMD_MESSAGE mtypes through `mtype2level`: 0=Info,
    /// 1=Warn, 2=Err, default (Fatal=3 and every unknown value)=Crit
    /// (pvaproto.h:704-712, serverconn.cpp:346-351). The tracing stack has
    /// no Crit, so Err and Crit collapse to its highest level, error!.
    /// A live-IOID mtype 0 must log at INFO (it was hidden at debug) and
    /// Fatal/unknown types at ERROR (also hidden at debug before). The
    /// unknown-IOID gate stays debug-only.
    #[test]
    fn r0604_message_severity_matches_mtype2level_for_live_ioid() {
        use std::sync::{Arc, Mutex};
        use tracing::Level;
        use tracing_subscriber::layer::{Context, Layer};
        use tracing_subscriber::prelude::*;

        struct LevelCapture(Arc<Mutex<Vec<Level>>>);
        impl<S: tracing::Subscriber> Layer<S> for LevelCapture {
            fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
                self.0.lock().unwrap().push(*event.metadata().level());
            }
        }

        let order = ByteOrder::Little;
        let peer = "127.0.0.1:5075".parse::<SocketAddr>().unwrap();

        // Build a channel with a live op for ioid 7 (constructed OUTSIDE
        // the capture scope so only handler events are recorded).
        let source: DynSource = Arc::new(crate::server_native::SharedSource::new());
        let mut ops = HashMap::new();
        ops.insert(
            7u32,
            non_monitor_op_state(
                std::sync::Arc::new(FieldDesc::Scalar(ScalarType::Int)),
                OpKind::Get,
                BitSet::new(),
            ),
        );
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            1,
            ChannelState {
                name: "dut:pv".into(),
                cid: 0,
                sid: 1,
                introspection: Some(std::sync::Arc::new(FieldDesc::Scalar(ScalarType::Int))),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops,
            },
        );

        let send = |ioid: u32, mtype: u8| {
            let mut payload = Vec::new();
            payload.put_u32(ioid, order);
            payload.put_u8(mtype);
            crate::proto::encode_string_into("m", order, &mut payload);
            let frame = synth_frame(Command::Message, order, payload);
            handle_message(&frame, &channels, &peer).expect("well-formed MESSAGE");
        };

        let levels = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(LevelCapture(levels.clone()));
        tracing::subscriber::with_default(subscriber, || {
            // Live IOID: 0->INFO, 1->WARN, 2->ERROR, 3->ERROR, 9->ERROR.
            for mtype in [0u8, 1, 2, 3, 9] {
                send(7, mtype);
            }
            // Unknown IOID stays DEBUG regardless of mtype.
            send(999, 1);
        });

        let got = levels.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![
                Level::INFO,
                Level::WARN,
                Level::ERROR,
                Level::ERROR,
                Level::ERROR,
                Level::DEBUG,
            ],
            "live-IOID mtypes 0/1/2/3/9 must map to INFO/WARN/ERROR/ERROR/ERROR; unknown IOID stays DEBUG"
        );
    }

    // Reactor-dependent, and specifically tokio-shaped: it stands a task up
    // with a bare `tokio::spawn` so it can hand `AbortOnDrop` a raw
    // `tokio::task::AbortHandle`, and it reads the outcome through tokio's
    // `JoinError::is_cancelled`. Under `rtems-exec-model` the `runtime::task`
    // seam's `TaskAbortHandle` is a *different* type with a different join
    // error, so the fixture does not even typecheck there — and the production
    // sites it stands in for (`finish_exec_data_task`, `monitor_abort`) take
    // their handle from the seam and do compile on both backends.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[epics_macros_rs::epics_test]
    async fn cancel_request_pauses_monitor_without_aborting() {
        // cancel-vs-destroy parity: pvxs serverconn.cpp:262-289
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

        let source: DynSource = Arc::new(crate::server_native::SharedSource::new());
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        let mut ops = HashMap::new();
        ops.insert(
            ioid,
            OpState {
                intro: std::sync::Arc::new(FieldDesc::Variant),
                kind: OpKind::Monitor,
                monitor_started: true,
                monitor_abort: Some(abort.clone()),
                mask: BitSet::new(),
                put_mask: None,
                monitor_window: None,
                monitor_window_notify: None,
                monitor_paused: paused.clone(),
                monitor_resume: Arc::new(tokio::sync::Notify::new()),
                monitor_wm: None,
                monitor_wm_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
                monitor_op_id: next_op_id(),
                monitor_filters: Arc::new(
                    epics_base_rs::server::database::filters::FilterChain::new(),
                ),
                pv_request: None,
                monitor_options: crate::server_native::source::MonitorOptions::default(),
                data_task_abort: None,
                monitor_start_ctl: None,
                exec_state: ExecState::Idle,
                last_request: false,
            },
        );
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 1,
                sid,
                introspection: None,
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops,
            },
        );

        // Build the CancelRequest payload: sid + ioid.
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        let frame = synth_frame(Command::CancelRequest, order, payload);
        handle_cancel_request(&frame, &mut channels).expect("well-formed CancelRequest");

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

    /// pvxs `serverconn.cpp:262-295` applies CANCEL_REQUEST to NON-monitor
    /// executing ops too: it sets the op `Idle`, the in-flight task's late
    /// reply is dropped (`serverget.cpp:37-49`), and a subsequent EXEC is
    /// accepted (`serverget.cpp:511-514`). Regression for the pre-fix
    /// handler that only paused monitors and left GET/PUT/RPC/PROCESS
    /// `Executing`, still able to emit a stale reply and blocking re-EXEC.
    // Same reason as `cancel_request_pauses_monitor_without_aborting` above:
    // a bare `tokio::spawn` handle into `AbortOnDrop` plus a tokio
    // `JoinError::is_cancelled` readout, neither of which exists on the
    // executor backend.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[epics_macros_rs::epics_test]
    async fn cancel_request_returns_non_monitor_exec_to_idle_and_aborts_task() {
        let order = ByteOrder::Little;
        let sid: u32 = 5;
        let ioid: u32 = 77;

        // A long-running task standing in for an in-flight GET data phase.
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
        let abort = Arc::new(AbortOnDrop(task.abort_handle()));

        let source: DynSource = Arc::new(crate::server_native::SharedSource::new());
        let mut op = non_monitor_op_state(
            std::sync::Arc::new(FieldDesc::Variant),
            OpKind::Get,
            BitSet::new(),
        );
        op.exec_state = ExecState::Executing;
        op.data_task_abort = Some(abort);
        op.last_request = true; // a last-request EXEC that is now cancelled
        let old_op_id = op.monitor_op_id;
        // The sticky destroy
        // marker must SURVIVE this cancel (asserted below).

        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        let mut ops = HashMap::new();
        ops.insert(ioid, op);
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(FieldDesc::Variant)),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops,
            },
        );

        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        let frame = synth_frame(Command::CancelRequest, order, payload);
        handle_cancel_request(&frame, &mut channels).expect("well-formed CancelRequest");

        let op = channels
            .get_mut(&sid)
            .and_then(|c| c.ops.get_mut(&ioid))
            .expect("op preserved across cancel — cancel is not a teardown");
        assert_eq!(
            op.exec_state,
            ExecState::Idle,
            "cancel must return the executing op to Idle"
        );
        assert!(
            op.data_task_abort.is_none(),
            "cancel must drop the abort guard so the in-flight task is aborted"
        );
        assert_ne!(
            op.monitor_op_id, old_op_id,
            "cancel must mint a fresh op-instance id so the stale ExecFinished is ignored"
        );
        assert!(
            op.last_request,
            "cancel must PRESERVE the sticky last_request marker (pvxs \
             serverconn.cpp:262-289 never clears ServerGPR::lastRequest); \
             the op survives Idle for re-EXEC and that EXEC's reply cleans it up"
        );

        // A subsequent EXEC is accepted now that the op is Idle.
        assert!(
            begin_exec(channels.get_mut(&sid).unwrap(), ioid).is_some(),
            "a second EXEC must be accepted after cancel (pvxs serverget.cpp:511-514)"
        );

        // Dropping the abort guard aborted the original in-flight task.
        let join = tokio::time::timeout(Duration::from_millis(500), task).await;
        let outcome = join.expect("aborted task should finish quickly");
        assert!(
            outcome.unwrap_err().is_cancelled(),
            "cancel must abort the in-flight non-monitor task"
        );
    }

    /// pvxs `CANCEL_REQUEST`
    /// (`serverconn.cpp:262-289`) sets an executing op `Idle` but never clears
    /// `ServerGPR::lastRequest`. A client may therefore send a last-request
    /// EXEC, cancel it before the source replies, then send a non-last EXEC:
    /// the sticky marker keeps `lastRequest` true on that next EXEC
    /// (`serverget.cpp:470-471`) so its `doReply` cleans the op up after
    /// replying (`serverget.cpp:111-114`). The pre-fix Rust handler cleared
    /// `last_request` on cancel, leaving the op live after the next reply —
    /// leaking an op pvxs would have released. This drives the lifecycle
    /// through the read-loop owner directly (no spawned tasks): cancel must
    /// preserve the marker, the stale canceled task's late completion must be
    /// ignored by the ABA guard, and the re-EXEC's completion must remove the
    /// op because the sticky marker survived.
    #[test]
    fn cancel_preserves_last_request_so_next_exec_completion_destroys_op() {
        let order = ByteOrder::Little;
        let sid: u32 = 9;
        let ioid: u32 = 123;

        let source: DynSource = Arc::new(crate::server_native::SharedSource::new());
        let mut op = non_monitor_op_state(
            std::sync::Arc::new(FieldDesc::Variant),
            OpKind::Get,
            BitSet::new(),
        );
        op.exec_state = ExecState::Executing;
        op.last_request = true; // a last-request EXEC, now about to be cancelled
        let stale_op_id = op.monitor_op_id;

        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        let mut ops = HashMap::new();
        ops.insert(ioid, op);
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(FieldDesc::Variant)),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops,
            },
        );

        // Cancel the in-flight last-request EXEC.
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        let frame = synth_frame(Command::CancelRequest, order, payload);
        handle_cancel_request(&frame, &mut channels).expect("well-formed CancelRequest");

        // The op survives Idle with the sticky marker preserved and a fresh
        // op-instance id.
        let live = &channels[&sid].ops[&ioid];
        assert_eq!(
            live.exec_state,
            ExecState::Idle,
            "cancel returns op to Idle"
        );
        assert!(live.last_request, "cancel must preserve the sticky marker");
        assert_ne!(
            live.monitor_op_id, stale_op_id,
            "cancel mints a fresh op-instance id"
        );

        // The canceled task's late ExecFinished (carrying the stale op id) is
        // ignored by the ABA guard: the op must NOT be removed here, despite
        // last_request being true.
        apply_exec_finish(
            &mut channels,
            ExecFinished {
                sid,
                ioid,
                op_id: stale_op_id,
                success: false,
            },
        );
        assert!(
            channels[&sid].ops.contains_key(&ioid),
            "stale completion (old op id) must not destroy the re-EXEC-able op"
        );

        // A subsequent EXEC is accepted; capture its op-instance id.
        let exec_id = begin_exec(channels.get_mut(&sid).unwrap(), ioid)
            .expect("a non-last EXEC is accepted after cancel");

        // That EXEC's reply completes SUCCESSFULLY. Because the sticky
        // last_request marker survived the cancel, a successful GPR reply makes
        // the completion owner remove the op — matching pvxs cleanup-after-reply
        // for a last-request op (serverget.cpp:111-114).
        apply_exec_finish(
            &mut channels,
            ExecFinished {
                sid,
                ioid,
                op_id: exec_id,
                success: true,
            },
        );
        assert!(
            !channels[&sid].ops.contains_key(&ioid),
            "the re-EXEC's reply must destroy the op (sticky last_request), \
             matching pvxs serverget.cpp:111-114"
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
        let bytes =
            build_monitor_payload(ioid, &intro, &PvField::Structure(value), None, &mask, order);
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

    /// The cooked payload builders ALWAYS write an empty (0x00) trailing
    /// overrun bitset, matching pvxs `servermon.cpp:174-176`, which
    /// hardcodes `to_wire(R, uint8_t(0u))` with `// TODO: placeholder for
    /// overrun mask` for every MONITOR DATA frame. Even on the marked
    /// (typed gateway / `+trigger`) path — where the server squashed a
    /// distinct intermediate value — the wire overrun bitset stays empty,
    /// so a pvxs client never sets `servSquash` / bumps `nSrvSquash`
    /// (`clientmon.cpp:554-564`) against this server. The decoded MONITOR
    /// DATA's trailing overrun bitset (`from_wire(M, overrun)`, surfaced
    /// here as `OpDataResponse::overrun`) must therefore have no bits set.
    #[test]
    fn cooked_payload_emits_empty_overrun_bitset() {
        let order = ByteOrder::Little;
        let ioid = 0x77;
        // { a: Double, b: Double } → root bit 0, a bit 1, b bit 2.
        let intro = FieldDesc::Structure {
            struct_id: "structure".into(),
            fields: vec![
                ("a".into(), FieldDesc::Scalar(ScalarType::Double)),
                ("b".into(), FieldDesc::Scalar(ScalarType::Double)),
            ],
        };
        let mut s = PvStructure::new("structure");
        s.fields
            .push(("a".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        s.fields
            .push(("b".into(), PvField::Scalar(ScalarValue::Double(2.0))));
        let value = PvField::Structure(s);
        let mask = BitSet::all_set(intro.total_bits());

        // `a` is the marked/+trigger leaf — the typed gateway path that
        // could carry server-side squash loss. The wire overrun bitset
        // must still be empty (pvxs placeholder).
        let marked = vec!["a".to_string()];
        let bytes = build_monitor_payload(ioid, &intro, &value, Some(&marked), &mask, order);
        let (frame, used) = try_parse_frame(&bytes).unwrap().expect("complete frame");
        assert_eq!(used, bytes.len());
        let data = match decode_op_response(&frame, Some(&intro)).unwrap() {
            OpResponse::Data(d) => d,
            other => panic!("expected monitor data, got {other:?}"),
        };
        assert!(
            data.overrun.iter().next().is_none(),
            "the cooked marked builder must emit an empty overrun bitset \
             (pvxs servermon.cpp:174-176 placeholder), got bits set"
        );

        // The plain full-value builder must likewise emit an empty
        // overrun bitset.
        let bytes = build_monitor_payload(ioid, &intro, &value, None, &mask, order);
        let (frame, _) = try_parse_frame(&bytes).unwrap().expect("complete frame");
        let data = match decode_op_response(&frame, Some(&intro)).unwrap() {
            OpResponse::Data(d) => d,
            other => panic!("expected monitor data, got {other:?}"),
        };
        assert!(
            data.overrun.iter().next().is_none(),
            "the cooked full-value builder must emit an empty overrun bitset"
        );
    }

    /// The two surviving monitor frame builders: a source that declares its
    /// marked leaves frames exactly those (pvxs `servermon.cpp:174`
    /// `to_wire_valid(R, ent, &pvMask)`), and a source that declares none
    /// frames the full request mask (pvxs's fully-marked `Value`). The port
    /// has no third, snapshot-diffing form — pvxs never reconstructs a marked
    /// set by comparing consecutive values, and the QSRV group monitor that
    /// once needed it now hands up its marked leaves on every event.
    ///
    /// A two-member group value with only `a` marked: the frame must mark
    /// `a`'s leaf and NOT `b`'s, while the full-mask builder marks both.
    #[test]
    fn br_r29_marked_monitor_payload_narrows_changed_bitset() {
        let order = ByteOrder::Little;
        let ioid = 0x29;
        // Group structure: { a: Double, b: Double }.
        let intro = FieldDesc::Structure {
            struct_id: "structure".into(),
            fields: vec![
                ("a".into(), FieldDesc::Scalar(ScalarType::Double)),
                ("b".into(), FieldDesc::Scalar(ScalarType::Double)),
            ],
        };
        let mk = |a: f64, b: f64| {
            let mut s = PvStructure::new("structure");
            s.fields
                .push(("a".into(), PvField::Scalar(ScalarValue::Double(a))));
            s.fields
                .push(("b".into(), PvField::Scalar(ScalarValue::Double(b))));
            PvField::Structure(s)
        };
        let curr = mk(9.0, 2.0);
        let mask = BitSet::all_set(intro.total_bits());

        // Marked builder: a self-trigger event on member `a` marks only `a`
        // (bit 1), never `b` (bit 2).
        let marked = vec!["a".to_string()];
        let narrowed = build_monitor_payload(ioid, &intro, &curr, Some(&marked), &mask, order);
        let (frame, _) = try_parse_frame(&narrowed).unwrap().expect("complete frame");
        let data = match decode_op_response(&frame, Some(&intro)).unwrap() {
            OpResponse::Data(d) => d,
            other => panic!("expected monitor data, got {other:?}"),
        };
        assert!(
            data.changed.get(1),
            "member `a` (bit 1) is marked — must be set"
        );
        assert!(
            !data.changed.get(2),
            "member `b` (bit 2) is not marked — must NOT be set (narrowing)"
        );

        // Full builder: a wildcard (all-set) request mask enumerates every
        // LEAF — {1, 2}, never the root bit. pvxs `to_wire_valid`
        // (dataencode.cpp:414-439) sets a wire bit only where
        // `store[bit].valid`, and `Value::mark` (data.cpp:256-270) never
        // validates a parent structure, so a root bit cannot appear.
        let full = build_monitor_payload(ioid, &intro, &curr, None, &mask, order);
        let (full_frame, _) = try_parse_frame(&full).unwrap().expect("complete frame");
        let full_data = match decode_op_response(&full_frame, Some(&intro)).unwrap() {
            OpResponse::Data(d) => d,
            other => panic!("expected monitor data, got {other:?}"),
        };
        assert_eq!(
            full_data.changed.iter().collect::<Vec<_>>(),
            vec![1, 2],
            "full-mask builder enumerates leaves, never the root structure \
             bit — pvxs to_wire_valid byte-exact (testxcode.cpp:111-116)"
        );
        match &full_data.value {
            PvField::Structure(s) => {
                assert_eq!(
                    s.get_field("a"),
                    Some(&PvField::Scalar(ScalarValue::Double(9.0)))
                );
                assert_eq!(
                    s.get_field("b"),
                    Some(&PvField::Scalar(ScalarValue::Double(2.0)))
                );
            }
            other => panic!("expected structure, got {other:?}"),
        }
    }

    /// R15-33: one framing rule for every value the server sends with a
    /// changed-bitset — MONITOR data, the MONITOR seed, the GET reply, the
    /// PUT_GET readback. pvxs frames all four with the same call,
    /// `to_wire_valid(R, value, &pvMask)` (`servermon.cpp:174`,
    /// `serverget.cpp:104`), over a value the source only partially assigned
    /// (`IOCSource::initialize` + `IOCSource::get` into a `cloneEmpty()`).
    ///
    /// So a read that declares its assigned leaves ([`SourceRead::marked`])
    /// frames THOSE, intersected with the request mask — not every leaf the
    /// mask selected. Both arms tested per boundary, plus the mask
    /// intersection that bounds either.
    #[test]
    fn read_changed_bitset_frames_only_the_leaves_the_source_assigned() {
        let intro = FieldDesc::Structure {
            struct_id: "structure".into(),
            fields: vec![
                ("a".into(), FieldDesc::Scalar(ScalarType::Double)),
                ("b".into(), FieldDesc::Scalar(ScalarType::Double)),
                ("c".into(), FieldDesc::Scalar(ScalarType::Double)),
            ],
        };
        let all = BitSet::all_set(intro.total_bits());

        // `marked: None` — a wholly-assigned value (pvxs's fully-marked
        // `Value`): every leaf the request selected, root bit excluded.
        assert_eq!(
            read_changed_bitset(&intro, &all, None)
                .iter()
                .collect::<Vec<_>>(),
            vec![1, 2, 3],
            "an unmarked read frames every selected leaf"
        );

        // `marked: Some` — only the assigned leaves reach the wire, which is
        // what makes a QSRV GET omit the seven leaves `getProperties` never
        // assigns (`testqsingle.cpp:129-149`).
        let assigned = vec!["a".to_string(), "c".to_string()];
        assert_eq!(
            read_changed_bitset(&intro, &all, Some(&assigned))
                .iter()
                .collect::<Vec<_>>(),
            vec![1, 3],
            "a declared read frames only the leaves the source assigned"
        );

        // The request mask still bounds it: a leaf the source assigned but the
        // client did not select is not framed (`… & pvMask`).
        let mut only_a = BitSet::new();
        only_a.set(1);
        assert_eq!(
            read_changed_bitset(&intro, &only_a, Some(&assigned))
                .iter()
                .collect::<Vec<_>>(),
            vec![1],
            "the request mask bounds the assigned set"
        );
    }

    /// W10-C2: byte-level pin on the connect-time monitor **seed** frame.
    ///
    /// The seed is queued with `marked: None` and no previous snapshot
    /// (`MonitorQueue::seed`), so the emitter dispatches it to
    /// [`build_monitor_payload`] with the op's full pvRequest mask. Its
    /// changed-bitset is therefore the leaf enumeration of that mask —
    /// pvxs `to_wire_valid` (`dataencode.cpp:414-439`) sets a wire bit only
    /// where `store[bit].valid`, and `Value::mark` (`data.cpp:256-270`)
    /// never validates a parent structure, so neither the root bit (0) nor
    /// the `alarm` (2) / `timeStamp` (6) bits can appear.
    /// `test/testxcode.cpp:111-116` is the upstream regression for this.
    ///
    /// Asserted byte-for-byte so a future bitset change cannot silently
    /// drift the seed frame.
    #[test]
    fn w10_c2_monitor_seed_frame_bytes_are_leaf_enumerated() {
        let order = ByteOrder::Big; // pvxs testxcode serializes big-endian
        let ioid = 1u32;
        // NTScalar<UInt32> exactly as pvxs's nt::NTScalar{TypeCode::UInt32}:
        // 0=root 1=value 2=alarm 3=.severity 4=.status 5=.message
        // 6=timeStamp 7=.secondsPastEpoch 8=.nanoseconds 9=.userTag
        let intro = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::UInt)),
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
                            ("userTag".into(), FieldDesc::Scalar(ScalarType::Int)),
                        ],
                    },
                ),
            ],
        };
        let mut alarm = PvStructure::new("alarm_t");
        alarm
            .fields
            .push(("severity".into(), PvField::Scalar(ScalarValue::Int(1))));
        alarm
            .fields
            .push(("status".into(), PvField::Scalar(ScalarValue::Int(2))));
        alarm.fields.push((
            "message".into(),
            PvField::Scalar(ScalarValue::String("hi".into())),
        ));
        let mut ts = PvStructure::new("time_t");
        ts.fields.push((
            "secondsPastEpoch".into(),
            PvField::Scalar(ScalarValue::Long(5)),
        ));
        ts.fields
            .push(("nanoseconds".into(), PvField::Scalar(ScalarValue::Int(6))));
        ts.fields
            .push(("userTag".into(), PvField::Scalar(ScalarValue::Int(7))));
        let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
        root.fields.push((
            "value".into(),
            PvField::Scalar(ScalarValue::UInt(0xdead_beef)),
        ));
        root.fields
            .push(("alarm".into(), PvField::Structure(alarm)));
        root.fields
            .push(("timeStamp".into(), PvField::Structure(ts)));
        let seed = PvField::Structure(root);

        // A wildcard pvRequest: `request_to_mask` selects every bit.
        let mask = BitSet::all_set(intro.total_bits());
        // The seed goes through the FIFO as `marked: None`, which the
        // emitter dispatches to `build_monitor_payload`.
        let mut q = MonitorQueue::new(4, &intro, &mask);
        q.seed(seed.clone().into());
        let ev = q.pop().expect("seed queued");
        assert!(ev.marked.is_none(), "the seed carries no explicit mark set");

        let frame =
            build_monitor_payload(ioid, &intro, &ev.value, ev.marked.as_deref(), &mask, order);
        // 8-byte PVA header, then the payload.
        let payload = &frame[8..];

        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            0x00, 0x00, 0x00, 0x01,             // ioid
            0x00,                               // subcmd (monitor data)
            // changed BitSet: 2 bytes, bits {1,3,4,5,7,8,9}.
            // byte0 = 1<<1|1<<3|1<<4|1<<5|1<<7 = 0xBA; byte1 = 1<<0|1<<1 = 0x03.
            0x02, 0xBA, 0x03,
            0xde, 0xad, 0xbe, 0xef,             // value: UInt 0xdeadbeef
            0x00, 0x00, 0x00, 0x01,             // alarm.severity = 1
            0x00, 0x00, 0x00, 0x02,             // alarm.status   = 2
            0x02, b'h', b'i',                   // alarm.message  = "hi"
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, // timeStamp.secondsPastEpoch = 5
            0x00, 0x00, 0x00, 0x06,             // timeStamp.nanoseconds = 6
            0x00, 0x00, 0x00, 0x07,             // timeStamp.userTag = 7
            0x00,                               // overrun BitSet: empty (servermon.cpp:174-176)
        ];
        assert_eq!(
            payload, expected,
            "connect-time monitor seed frame bytes (leaf-enumerated \
             changed-bitset; pvxs testxcode.cpp:111-116)"
        );
    }

    /// pvxs `servermon.cpp:493` parity: when the client sets the
    /// pipeline bit (`subcmd & 0x80`) on MONITOR INIT, the body
    /// carries a trailing u32 `nack` (initial window). The handler
    /// must consume those four bytes so subsequent reads from the
    /// cursor see the correct offset, AND surface the parsed value
    /// to override the pvRequest queueSize-based default.
    #[test]
    fn parse_monitor_init_nack_consumes_window_and_faults_on_truncation() {
        let order = ByteOrder::Little;

        // Bit clear → Ok(None) even on Monitor; cursor untouched. The
        // call site seeds the credit window with `None.unwrap_or(0)` == 0
        // (pvxs `nack = 0` default + `op->window = nack`), NOT the
        // pvRequest queueSize — an absent rider stalls the pipeline until
        // the first MONITOR_ACK rather than pre-crediting queueSize frames.
        let bytes = [0u8; 8];
        let mut cur = std::io::Cursor::new(bytes.as_slice());
        let absent = parse_monitor_init_nack(OpKind::Monitor, 0x08, &mut cur, order).unwrap();
        assert_eq!(absent, None);
        assert_eq!(
            absent.unwrap_or(0),
            0,
            "absent rider must seed window 0, not queueSize"
        );
        assert_eq!(cur.position(), 0, "cursor must not advance when bit clear");

        // Bit set, kind != Monitor → Ok(None) (matches pvxs which only
        // honours the pipeline shape on the MONITOR command code).
        let mut cur = std::io::Cursor::new(bytes.as_slice());
        assert_eq!(
            parse_monitor_init_nack(OpKind::Get, 0x88, &mut cur, order).unwrap(),
            None
        );
        assert_eq!(cur.position(), 0);

        // Bit set, four bytes available → Ok(Some(value)), advance 4.
        let mut buf = Vec::new();
        buf.put_u32(0x1234_5678, order);
        buf.extend_from_slice(b"trailing");
        let mut cur = std::io::Cursor::new(buf.as_slice());
        assert_eq!(
            parse_monitor_init_nack(OpKind::Monitor, 0x88, &mut cur, order).unwrap(),
            Some(0x1234_5678)
        );
        assert_eq!(cur.position(), 4, "must advance exactly four bytes");

        // Bit set, fewer than four bytes → FATAL decode error. pvxs reads
        // the nack unconditionally once the bit is set and resets the
        // connection on `!M.good()` (servermon.cpp:494-503); there is no
        // silent fallback to the pvRequest queueSize default.
        let buf = vec![0x11, 0x22];
        let mut cur = std::io::Cursor::new(buf.as_slice());
        let err = parse_monitor_init_nack(OpKind::Monitor, 0x88, &mut cur, order)
            .expect_err("truncated nack with pipeline bit set must be fatal");
        assert!(
            matches!(err, PvaError::Decode(_)),
            "expected a Decode error, got {err:?}"
        );
    }

    /// an RPC EXEC argument body must classify as parameterless,
    /// fully decoded, or fatally malformed — never silently fabricate a
    /// `Null` argument from a present-but-undecodable body. pvxs
    /// `serverget.cpp:443-447` decodes `from_wire_type_value` and
    /// `serverget.cpp:454-458` `bev.reset()`s the connection on
    /// `!M.good()`.
    #[test]
    fn decode_rpc_exec_arg_parameterless_vs_malformed() {
        let order = ByteOrder::Little;

        // Absent body (no payload after subcmd) → parameterless; cursor
        // does not advance.
        let mut cur = std::io::Cursor::new([].as_slice());
        assert_eq!(
            decode_rpc_exec_arg(&mut cur, order, &mut TypeCache::new()).unwrap(),
            (FieldDesc::Variant, PvField::Null)
        );
        assert_eq!(cur.position(), 0);

        // NULL (0xFF) type code → parameterless; consume exactly that one
        // byte even when trailing bytes follow. pvxs encodes a
        // parameterless RPC as the single 0xFF byte produced by
        // `clientget.cpp:308` `to_wire(R, desc(arg))` for a null arg.
        let buf = [0xFFu8, 0xAA, 0xBB];
        let mut cur = std::io::Cursor::new(buf.as_slice());
        assert_eq!(
            decode_rpc_exec_arg(&mut cur, order, &mut TypeCache::new()).unwrap(),
            (FieldDesc::Variant, PvField::Null)
        );
        assert_eq!(
            cur.position(),
            1,
            "only the 0xFF type-code byte is consumed"
        );

        // Present, fully decodable descriptor + value round-trips to the
        // exact argument.
        let desc = FieldDesc::Scalar(ScalarType::Int);
        let value = PvField::Scalar(ScalarValue::Int(0x1234_5678));
        let mut wire = Vec::new();
        encode_type_desc(&desc, order, &mut wire);
        let desc_len = wire.len();
        encode_pv_field(&value, &desc, order, &mut wire);
        let mut cur = std::io::Cursor::new(wire.as_slice());
        assert_eq!(
            decode_rpc_exec_arg(&mut cur, order, &mut TypeCache::new()).unwrap(),
            (desc.clone(), value.clone())
        );
        assert_eq!(cur.position() as usize, wire.len());

        // Present-but-truncated descriptor (a structure tag with no body)
        // is a connection-fatal decode error, not a parameterless call.
        let buf = [0x80u8]; // TAG_STRUCTURE, no id/field body
        let mut cur = std::io::Cursor::new(buf.as_slice());
        decode_rpc_exec_arg(&mut cur, order, &mut TypeCache::new())
            .expect_err("truncated RPC descriptor must be fatal");

        // Valid descriptor plus a truncated value (a 4-byte Int cut to
        // two bytes) is also fatal.
        let truncated = &wire[..desc_len + 2];
        let mut cur = std::io::Cursor::new(truncated);
        decode_rpc_exec_arg(&mut cur, order, &mut TypeCache::new())
            .expect_err("truncated RPC value must be fatal");
    }

    /// pvxs `serverchan.cpp:382-386`: when the SID in DESTROY_CHANNEL
    /// is unknown the server logs at debug and silently returns — no
    /// reply frame is emitted. Previously we unconditionally fabricated
    /// `OK` echo back even for SIDs we never created, which both
    /// amplifies (1:1) and confuses correctness diagnostics in the
    /// client.
    #[epics_macros_rs::epics_test]
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

        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let peer_entry = crate::server_native::peers::PeerEntry::new(false);
        let teardown = ChannelTeardownCtx {
            tx: &tx,
            order,
            peer,
            peer_entry: &peer_entry,
        };
        handle_destroy_channel(&frame, &mut channels, &teardown)
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
    #[epics_macros_rs::epics_test]
    async fn destroy_channel_on_known_sid_emits_echo() {
        let order = ByteOrder::Little;
        let sid: u32 = 11;
        let cid: u32 = 22;

        let source: DynSource = Arc::new(crate::server_native::SharedSource::new());
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid,
                sid,
                introspection: None,
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);

        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(cid, order);
        let frame = synth_frame(Command::DestroyChannel, order, payload);

        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let peer_entry = crate::server_native::peers::PeerEntry::new(false);
        let teardown = ChannelTeardownCtx {
            tx: &tx,
            order,
            peer,
            peer_entry: &peer_entry,
        };
        handle_destroy_channel(&frame, &mut channels, &teardown)
            .await
            .expect("handler returns Ok");

        assert!(!channels.contains_key(&sid), "channel removed on hit");
        let reply = rx.try_recv().expect("reply emitted for known SID");
        // Header (8) + ioid placeholder isn't part of DESTROY_CHANNEL;
        // payload is sid (4) + cid (4) = 8 total, so frame length = 16.
        assert_eq!(reply.len(), PvaHeader::SIZE + 8);
    }

    /// Inbound application frames must be decoded with the byte order in
    /// the frame's own header, not the server's configured outbound order
    /// (pvxs latches `peerBE` per received message, conn.cpp:195-198, and
    /// builds every handler's input buffer from it, serverchan.cpp:262-373).
    /// Here a big-endian-encoded DESTROY_CHANNEL reaches a server whose
    /// configured (outbound) order is little-endian: the SID/CID must still
    /// decode correctly so the channel is found and removed. Before the fix
    /// the handler decoded the payload with the little-endian config order,
    /// byte-swapping the SID into a value no channel owned — silently
    /// dropping the request.
    #[epics_macros_rs::epics_test]
    async fn handle_destroy_channel_decodes_with_frame_header_order_not_config() {
        let config_order = ByteOrder::Little;
        let inbound_order = ByteOrder::Big;
        let sid: u32 = 0x0102_0304;
        let cid: u32 = 0x0506_0708;

        let source: DynSource = Arc::new(crate::server_native::SharedSource::new());
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid,
                sid,
                introspection: None,
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);

        // Payload encoded big-endian; header flagged big-endian.
        let mut payload = Vec::new();
        payload.put_u32(sid, inbound_order);
        payload.put_u32(cid, inbound_order);
        let frame = synth_frame(Command::DestroyChannel, inbound_order, payload);

        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let peer_entry = crate::server_native::peers::PeerEntry::new(false);
        // Server configured little-endian (config_order) for its OWN
        // outbound frames; the inbound frame is big-endian.
        let teardown = ChannelTeardownCtx {
            tx: &tx,
            order: config_order,
            peer,
            peer_entry: &peer_entry,
        };
        handle_destroy_channel(&frame, &mut channels, &teardown)
            .await
            .expect("handler returns Ok");

        assert!(
            !channels.contains_key(&sid),
            "BE-encoded SID must decode via the frame header order and find the channel"
        );
        let reply = rx.try_recv().expect("reply emitted for the decoded SID");
        assert_eq!(reply.len(), PvaHeader::SIZE + 8);
    }

    /// Same per-frame-order rule for a data operation: a MONITOR ACK
    /// encoded big-endian against a little-endian-configured server must
    /// decode its SID/IOID/ack-count from the frame header order. Before
    /// the fix the SID/IOID byte-swapped, the ACK matched no live op, and
    /// the credit window was never refilled.
    #[epics_macros_rs::epics_test]
    async fn monitor_ack_decodes_with_frame_header_order_not_config() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let config_order = ByteOrder::Little;
        let inbound_order = ByteOrder::Big;
        let sid: u32 = 3;
        let ioid: u32 = 88;

        let window = Arc::new(AtomicU32::new(0));
        let source: DynSource = Arc::new(crate::server_native::SharedSource::new());
        let mut channels = ack_test_channels(sid, ioid, window.clone(), source.clone());

        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let config = crate::server_native::runtime::PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        let mut payload = Vec::new();
        payload.put_u32(sid, inbound_order);
        payload.put_u32(ioid, inbound_order);
        payload.put_u8(0x80);
        payload.put_u32(3, inbound_order); // ack-count, big-endian
        let frame = synth_frame(Command::Monitor, inbound_order, payload);
        handle_op(
            &frame,
            &tx,
            &mut channels,
            config_order,
            &fixed_out_order(config_order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("well-formed BE MONITOR ACK ok");
        assert_eq!(
            window.load(Ordering::Relaxed),
            3,
            "BE ack-count must decode via the frame header order and refill the window"
        );
    }

    /// pvxs `ServerChan::cleanup` (serverchan.cpp:43-60, :115-127) runs
    /// the per-op teardown and then invokes the channel's `onClose`
    /// exactly once. The Rust server must deliver the same edge to the
    /// bound `ChannelSource` via `notify_channel_close` — on explicit
    /// DESTROY_CHANNEL here, and (by routing both paths through
    /// `close_channel`) on connection teardown. Before the hook existed a
    /// source could never observe a channel closing: per-channel leases,
    /// upstream identities, and credential-scoped caches leaked for the
    /// life of the process.
    #[epics_macros_rs::epics_test]
    async fn destroy_channel_notifies_bound_source_once() {
        struct RecordingCloseSource {
            closed: Arc<parking_lot::Mutex<Vec<String>>>,
        }
        impl crate::server_native::source::ChannelSource for RecordingCloseSource {
            async fn list_pvs(&self) -> Vec<String> {
                vec!["dut".into()]
            }
            async fn has_pv(&self, name: &str) -> bool {
                name == "dut"
            }
            async fn get_introspection(&self, _name: &str) -> Option<FieldDesc> {
                Some(FieldDesc::Variant)
            }
            async fn get_value(&self, _name: &str) -> Option<PvField> {
                None
            }
            async fn put_value(&self, _name: &str, _value: PvField) -> Result<(), OpError> {
                Ok(())
            }
            async fn is_writable(&self, _name: &str) -> bool {
                false
            }
            async fn subscribe(
                &self,
                _name: &str,
            ) -> Option<crate::server_native::MonitorStream<PvField>> {
                None
            }
            fn notify_channel_close(
                &self,
                name: &str,
                _ctx: &crate::server_native::source::ChannelContext,
            ) {
                self.closed.lock().push(name.to_string());
            }
        }

        let order = ByteOrder::Little;
        let sid: u32 = 5;
        let cid: u32 = 6;
        let closed = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let source: DynSource = Arc::new(RecordingCloseSource {
            closed: closed.clone(),
        });
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid,
                sid,
                introspection: None,
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );
        let (tx, mut _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);

        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(cid, order);
        let frame = synth_frame(Command::DestroyChannel, order, payload);

        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let peer_entry = crate::server_native::peers::PeerEntry::new(false);
        let teardown = ChannelTeardownCtx {
            tx: &tx,
            order,
            peer,
            peer_entry: &peer_entry,
        };
        handle_destroy_channel(&frame, &mut channels, &teardown)
            .await
            .expect("handler returns Ok");

        assert_eq!(
            *closed.lock(),
            vec!["dut".to_string()],
            "DESTROY_CHANNEL must fire notify_channel_close exactly once for the channel name"
        );

        // A second destroy of the now-absent SID is a no-op (pvxs unknown-
        // SID silence) and must NOT re-fire the close hook.
        let mut payload2 = Vec::new();
        payload2.put_u32(sid, order);
        payload2.put_u32(cid, order);
        let frame2 = synth_frame(Command::DestroyChannel, order, payload2);
        handle_destroy_channel(&frame2, &mut channels, &teardown)
            .await
            .expect("handler returns Ok");
        assert_eq!(
            *closed.lock(),
            vec!["dut".to_string()],
            "a destroy on an already-removed SID must not re-fire onClose"
        );
    }

    /// Build a `ca`-method credential for the given account, used by the
    /// channel-lifecycle credential-pinning regressions below.
    fn cred_ca(account: &str) -> ClientCredentials {
        ClientCredentials {
            method: "ca".into(),
            account: account.into(),
            host: "10.0.0.1".into(),
            authority: String::new(),
            roles: Vec::new(),
        }
    }

    /// Records the `(method, account)` each edge observed: channel open
    /// (`notify_channel_open`), channel close (`notify_channel_close`), and
    /// the per-op GET ACF check (`get_value_checked`). Lets these
    /// regression tests assert which
    /// identity each edge ran under.
    struct CredRecordingSource {
        opened: Arc<parking_lot::Mutex<Vec<(String, String)>>>,
        closed: Arc<parking_lot::Mutex<Vec<(String, String)>>>,
        op_reads: Arc<parking_lot::Mutex<Vec<(String, String)>>>,
        intro: FieldDesc,
        value: PvField,
    }
    impl crate::server_native::source::ChannelSource for CredRecordingSource {
        async fn list_pvs(&self) -> Vec<String> {
            vec!["dut".into()]
        }
        async fn has_pv(&self, name: &str) -> bool {
            name == "dut"
        }
        async fn get_introspection(&self, _name: &str) -> Option<FieldDesc> {
            Some(self.intro.clone())
        }
        async fn get_value(&self, _name: &str) -> Option<PvField> {
            Some(self.value.clone())
        }
        async fn get_value_checked(
            &self,
            checked: crate::server_native::source::AccessChecked,
            ctx: crate::server_native::source::ChannelContext,
        ) -> Option<PvField> {
            self.op_reads
                .lock()
                .push((ctx.method.clone(), ctx.account.clone()));
            if !checked.allows_read() {
                return None;
            }
            Some(self.value.clone())
        }
        async fn put_value(&self, _name: &str, _value: PvField) -> Result<(), OpError> {
            Ok(())
        }
        async fn is_writable(&self, _name: &str) -> bool {
            false
        }
        async fn subscribe(&self, _name: &str) -> Option<MonitorStream<PvField>> {
            None
        }
        fn notify_channel_open(
            &self,
            _name: &str,
            ctx: &crate::server_native::source::ChannelContext,
        ) {
            self.opened
                .lock()
                .push((ctx.method.clone(), ctx.account.clone()));
        }
        fn notify_channel_close(
            &self,
            _name: &str,
            ctx: &crate::server_native::source::ChannelContext,
        ) {
            self.closed
                .lock()
                .push((ctx.method.clone(), ctx.account.clone()));
        }
    }

    fn empty_cred_log() -> Arc<parking_lot::Mutex<Vec<(String, String)>>> {
        Arc::new(parking_lot::Mutex::new(Vec::new()))
    }

    /// Open edge.
    ///
    /// pvxs builds the channel's `ServerChannelControl` with `conn->cred` at
    /// CREATE_CHANNEL (serverchan.cpp:62) and runs the source attach under
    /// that captured identity; a later `ServerConn::cred` reassignment does
    /// not rewrite it. So a client that CREATEs under `alice/ca` and
    /// re-authenticates to `bob/ca` before the async resolver completes must
    /// still see `notify_channel_open` fire as Alice. `handle_create_channel`
    /// snapshots the dispatch-time credential into
    /// `CreateChannelCompletion::open_cred`, and the read loop fires the open
    /// callback from the channel's stored `open_cred`, never the current `cred`.
    #[epics_macros_rs::epics_test]
    async fn reauth_channel_open_callback_pinned_to_create_credential() {
        let order = ByteOrder::Little;
        let opened = empty_cred_log();
        let source: DynSource = Arc::new(CredRecordingSource {
            opened: opened.clone(),
            closed: empty_cred_log(),
            op_reads: empty_cred_log(),
            intro: FieldDesc::Variant,
            value: PvField::Scalar(ScalarValue::Int(7)),
        });

        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let (cc_tx, mut cc_rx) = tokio::sync::mpsc::channel::<CreateChannelCompletion>(8);
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        let mut pending = 0usize;

        // CREATE_CHANNEL for "dut", dispatched while the connection identity
        // is alice/ca.
        let cid: u32 = 42;
        let mut payload = Vec::new();
        payload.put_u16(1, order);
        payload.put_u32(cid, order);
        encode_string_into("dut", order, &mut payload);
        let frame = synth_frame(Command::CreateChannel, order, payload);

        let alice = cred_ca("alice");
        handle_create_channel(
            &source,
            &frame,
            &tx,
            &channels,
            order,
            100,
            peer,
            &alice,
            &cc_tx,
            &mut pending,
        )
        .await
        .expect("CREATE_CHANNEL dispatch ok");

        let completion = cc_rx.recv().await.expect("resolver emits a completion");
        assert_eq!(
            (
                completion.open_cred.method.as_str(),
                completion.open_cred.account.as_str()
            ),
            ("ca", "alice"),
            "the completion must carry the credential in force at CREATE dispatch (alice)"
        );
        let resolved = completion
            .resolved
            .expect("the recording source resolves \"dut\"");

        // The connection re-authenticates to bob/ca BEFORE the read loop
        // applies the completion; the open callback must still fire as alice.
        let _bob = cred_ca("bob");
        // Mirrors the read loop's completion arm (tcp.rs ~2895-2922): the
        // channel stores the completion's open_cred and the open callback is
        // built from `ch.open_cred`, never the (now bob) connection credential.
        channels.insert(
            completion.sid,
            ChannelState {
                name: completion.name.clone(),
                cid: completion.cid,
                sid: completion.sid,
                introspection: resolved.intro,
                source: resolved.owner,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: completion.open_cred,
                ops: HashMap::new(),
            },
        );
        let ch = channels.get(&completion.sid).unwrap();
        let ctx = channel_lifecycle_ctx(peer, &ch.open_cred);
        ch.source.notify_channel_open(&ch.name, &ctx);

        assert_eq!(
            *opened.lock(),
            vec![("ca".to_string(), "alice".to_string())],
            "notify_channel_open must run under the channel's CREATE-time identity (alice), \
             not the re-authed connection identity (bob)"
        );
    }

    /// Client-DESTROY
    /// close edge. A channel opened under `alice/ca` and destroyed by a client
    /// DESTROY_CHANNEL after the connection re-authed to `bob/ca` must deliver
    /// `notify_channel_close` under Alice — the close identity comes from the
    /// channel's stored `open_cred`, not the connection's current credential.
    #[epics_macros_rs::epics_test]
    async fn reauth_client_destroy_close_pinned_to_create_credential() {
        let order = ByteOrder::Little;
        let sid: u32 = 7;
        let cid: u32 = 9;
        let closed = empty_cred_log();
        let source: DynSource = Arc::new(CredRecordingSource {
            opened: empty_cred_log(),
            closed: closed.clone(),
            op_reads: empty_cred_log(),
            intro: FieldDesc::Variant,
            value: PvField::Scalar(ScalarValue::Int(0)),
        });
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid,
                sid,
                introspection: None,
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                // Channel was created under alice/ca.
                open_cred: cred_ca("alice"),
                ops: HashMap::new(),
            },
        );

        let (tx, mut _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(cid, order);
        let frame = synth_frame(Command::DestroyChannel, order, payload);

        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let peer_entry = crate::server_native::peers::PeerEntry::new(false);
        // The ChannelTeardownCtx carries NO credential post-fix; the close
        // identity comes from the channel's open_cred even though the
        // connection has since re-authed to bob.
        let teardown = ChannelTeardownCtx {
            tx: &tx,
            order,
            peer,
            peer_entry: &peer_entry,
        };
        handle_destroy_channel(&frame, &mut channels, &teardown)
            .await
            .expect("destroy ok");

        assert_eq!(
            *closed.lock(),
            vec![("ca".to_string(), "alice".to_string())],
            "client DESTROY_CHANNEL close callback must use the CREATE-time identity (alice)"
        );
    }

    /// Server-teardown
    /// close edge. The connection-teardown / operator `:drop`/`:flush` path
    /// tears channels down through `finalize_channel_destroy` (server-initiated
    /// DESTROY_CHANNEL). It must also deliver `notify_channel_close` under the
    /// channel's CREATE-time identity, not whatever the connection last
    /// re-authed to.
    #[epics_macros_rs::epics_test]
    async fn reauth_server_teardown_close_pinned_to_create_credential() {
        let order = ByteOrder::Little;
        let sid: u32 = 3;
        let cid: u32 = 4;
        let closed = empty_cred_log();
        let source: DynSource = Arc::new(CredRecordingSource {
            opened: empty_cred_log(),
            closed: closed.clone(),
            op_reads: empty_cred_log(),
            intro: FieldDesc::Variant,
            value: PvField::Scalar(ScalarValue::Int(0)),
        });
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid,
                sid,
                introspection: None,
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: cred_ca("alice"),
                ops: HashMap::new(),
            },
        );

        let (tx, mut _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let peer_entry = crate::server_native::peers::PeerEntry::new(false);
        let teardown = ChannelTeardownCtx {
            tx: &tx,
            order,
            peer,
            peer_entry: &peer_entry,
        };

        let torn = finalize_channel_destroy(
            sid,
            cid,
            DestroyCause::ServerInitiated,
            &mut channels,
            &teardown,
        )
        .await;
        assert!(torn, "the channel was present and torn down");
        assert_eq!(
            *closed.lock(),
            vec![("ca".to_string(), "alice".to_string())],
            "server-initiated teardown close callback must use the CREATE-time identity (alice)"
        );
    }

    /// Per-op edge
    /// (the converse guard). Only the channel *lifecycle* edges are pinned to
    /// the CREATE-time credential; per-operation handlers must keep using the
    /// connection's *current* credential (pvxs builds each `ConnectOp`/`ExecOp`
    /// from `conn->cred`). A GET on a channel opened under `alice/ca`, executed
    /// after the connection re-authed to `bob/ca`, must run its ACF check as
    /// Bob — proving the fix did not over-pin operations to `open_cred`.
    #[epics_macros_rs::epics_test]
    async fn reauth_get_op_uses_current_connection_credential() {
        let order = ByteOrder::Little;
        let sid: u32 = 11;
        let ioid: u32 = 21;
        let intro = FieldDesc::Scalar(ScalarType::Int);
        let op_reads = empty_cred_log();
        let source: DynSource = Arc::new(CredRecordingSource {
            opened: empty_cred_log(),
            closed: empty_cred_log(),
            op_reads: op_reads.clone(),
            intro: intro.clone(),
            value: PvField::Scalar(ScalarValue::Int(7)),
        });

        let mut ops: HashMap<u32, OpState> = HashMap::new();
        ops.insert(
            ioid,
            non_monitor_op_state(
                std::sync::Arc::new(intro.clone()),
                OpKind::Get,
                BitSet::all_set(intro.total_bits()),
            ),
        );
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                // Channel was created under alice/ca.
                open_cred: cred_ca("alice"),
                ops,
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = crate::server_native::runtime::PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        // Connection has since re-authenticated to bob/ca.
        let bob = cred_ca("bob");

        // GET EXEC (subcmd 0x40) under the current connection credential bob.
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x40);
        let frame = synth_frame(Command::Get, order, payload);
        handle_op(
            &frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Get,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &bob,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("GET EXEC ok");
        let _ = rx.recv().await.expect("GET data reply emitted");

        assert_eq!(
            *op_reads.lock(),
            vec![("ca".to_string(), "bob".to_string())],
            "the per-op GET ACF check must use the connection's CURRENT credential (bob), \
             not the channel's CREATE-time open_cred (alice)"
        );
    }

    /// A source's out-of-band channel invalidation (PVA gateway
    /// operator `:drop`/`:flush`) force-disconnects exactly the downstream
    /// channels serving the named PV with a server-initiated DESTROY_CHANNEL,
    /// leaving channels under other names untouched. Before this fix a
    /// drop/flush only ended the upstream monitor; the downstream channel
    /// lingered and silently rebound to a re-created cache entry on the next
    /// event. Asserts the single-owner teardown removed only the match, sent
    /// a DESTROY_CHANNEL addressing it, and released only its report count.
    #[epics_macros_rs::epics_test]
    async fn invalidate_named_channels_force_disconnects_only_matching_name() {
        let order = ByteOrder::Little;
        let source: DynSource = Arc::new(crate::server_native::SharedSource::new());

        // Two live channels: "X" (sid 1) is the invalidation target; "Y"
        // (sid 2), a different name, must survive.
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        let peer_entry = crate::server_native::peers::PeerEntry::new(false);
        for (sid, cid, name) in [(1u32, 10u32, "X"), (2u32, 20u32, "Y")] {
            let stat = crate::server_native::peers::ChannelStat::new(name.into());
            channels.insert(
                sid,
                ChannelState {
                    name: name.into(),
                    cid,
                    sid,
                    introspection: None,
                    source: source.clone(),
                    stat: stat.clone(),
                    open_cred: ClientCredentials::anonymous(TEST_PEER),
                    ops: HashMap::new(),
                },
            );
            peer_entry.channel_opened(sid, stat);
        }
        assert_eq!(
            peer_entry
                .channels
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let teardown = ChannelTeardownCtx {
            tx: &tx,
            order,
            peer,
            peer_entry: &peer_entry,
        };

        let torn = invalidate_named_channels("X", &mut channels, &teardown).await;

        assert_eq!(torn, 1, "exactly the one channel named X is torn down");
        assert!(!channels.contains_key(&1), "X (sid 1) removed");
        assert!(channels.contains_key(&2), "Y (sid 2) survives a drop of X");
        assert_eq!(
            peer_entry
                .channels
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "only X's live-channel count is released"
        );

        // A server-initiated DESTROY_CHANNEL addressing sid 1 / cid 10 was
        // emitted (header + sid + cid, little-endian).
        let frame = rx.try_recv().expect("DESTROY_CHANNEL frame emitted for X");
        assert_eq!(frame.len(), PvaHeader::SIZE + 8);
        let body = &frame[PvaHeader::SIZE..];
        let got_sid = u32::from_le_bytes(body[0..4].try_into().unwrap());
        let got_cid = u32::from_le_bytes(body[4..8].try_into().unwrap());
        assert_eq!(
            (got_sid, got_cid),
            (1, 10),
            "the emitted frame addresses the X channel"
        );
        assert!(
            rx.try_recv().is_err(),
            "no frame emitted for the surviving Y channel"
        );
    }

    /// A published name this connection does not serve is a
    /// no-op — no teardown, no frame. The invalidation broadcast is
    /// server-wide, so most connections hold no channel under a given
    /// dropped name and must not be disturbed.
    #[epics_macros_rs::epics_test]
    async fn invalidate_named_channels_unknown_name_is_noop() {
        let order = ByteOrder::Little;
        let source: DynSource = Arc::new(crate::server_native::SharedSource::new());
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        let peer_entry = crate::server_native::peers::PeerEntry::new(false);
        let stat = crate::server_native::peers::ChannelStat::new("X".into());
        channels.insert(
            1,
            ChannelState {
                name: "X".into(),
                cid: 10,
                sid: 1,
                introspection: None,
                source: source.clone(),
                stat: stat.clone(),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );
        peer_entry.channel_opened(1, stat);

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let teardown = ChannelTeardownCtx {
            tx: &tx,
            order,
            peer,
            peer_entry: &peer_entry,
        };

        let torn = invalidate_named_channels("Z", &mut channels, &teardown).await;
        assert_eq!(torn, 0);
        assert!(channels.contains_key(&1), "non-matching channel untouched");
        assert_eq!(
            peer_entry
                .channels
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(
            rx.try_recv().is_err(),
            "no frame emitted for a name this connection does not serve"
        );
    }

    /// pvxs `serverget.cpp:83` echoes the request `subcmd` byte in the
    /// PUT data response. The PUT_GET (readback) case sets bit 0x40 in
    /// the client subcmd; pvxs `clientget.cpp:362-370` dispatches the
    /// reply decode based on that bit. A server response that hardcodes
    /// 0x00 makes the client decode the wrong shape: the bitset + value
    /// bytes carried in the frame are misread as trailing garbage and
    /// the PUT_GET readback is silently lost.
    #[epics_macros_rs::epics_test]
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
        let pv = SharedPV::build_mailbox();
        let intro = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let mut initial = PvStructure::new("epics:nt/NTScalar:1.0");
        initial
            .fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        pv.open(intro.clone(), PvField::Structure(initial)).unwrap();

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
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

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
            &init_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Put,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
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
            &exec_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Put,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("PUT EXEC ok");

        let resp = rx.recv().await.expect("PUT EXEC response emitted");
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
    #[epics_macros_rs::epics_test]
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

        let pv = SharedPV::build_mailbox();
        let intro = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let mut initial = PvStructure::new("epics:nt/NTScalar:1.0");
        initial
            .fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        pv.open(intro.clone(), PvField::Structure(initial)).unwrap();

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
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

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
            &init_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Put,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
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
            &exec_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Put,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("PUT EXEC ok");
        let resp = rx.recv().await.expect("PUT EXEC response emitted");
        assert!(resp.len() >= PvaHeader::SIZE + 5);
        let resp_subcmd = resp[PvaHeader::SIZE + 4];
        assert_eq!(
            resp_subcmd, 0x00,
            "plain PUT EXEC reply subcmd must echo 0x00"
        );
    }

    /// a GET EXEC that sets the last-request bit (`subcmd & 0x10`)
    /// keeps the op (and its IOID) reserved while the response task runs and
    /// only removes it once the completion signal is applied — matching pvxs
    /// `cleanup()` *after* `doReply` (`serverget.cpp:111-114`), recorded from
    /// `lastRequest = subcmd & 0x10` (`serverget.cpp:470-471`). Freeing the
    /// IOID at spawn time would let a re-INIT racing a slow source collide
    /// with the still-in-flight reply on one IOID. A plain GET EXEC (no
    /// `0x10`) returns the op to `Idle` on completion.
    #[epics_macros_rs::epics_test]
    async fn get_exec_last_request_defers_op_removal_until_response_sent() {
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
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();

        let intro = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let pv = SharedPV::new();
        let mut initial = PvStructure::new("epics:nt/NTScalar:1.0");
        initial
            .fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(2.5))));
        pv.open(intro.clone(), PvField::Structure(initial)).unwrap();
        let shared = SharedSource::new();
        shared.add("dut", pv);
        let source: DynSource = Arc::new(shared);

        let config = PvaServerConfig::default();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // A channel carrying one already-INIT'd GET op.
        let make_channels = || {
            let mut ops: HashMap<u32, OpState> = HashMap::new();
            ops.insert(
                ioid,
                non_monitor_op_state(
                    std::sync::Arc::new(intro.clone()),
                    OpKind::Get,
                    BitSet::all_set(intro.total_bits()),
                ),
            );
            let mut channels: HashMap<u32, ChannelState> = HashMap::new();
            channels.insert(
                sid,
                ChannelState {
                    name: "dut".into(),
                    cid: 0,
                    sid,
                    introspection: Some(std::sync::Arc::new(intro.clone())),
                    source: source.clone(),
                    stat: crate::server_native::peers::ChannelStat::new(String::new()),
                    open_cred: ClientCredentials::anonymous(TEST_PEER),
                    ops,
                },
            );
            channels
        };

        let exec_frame = |subcmd: u8| {
            let mut payload = Vec::new();
            payload.put_u32(sid, order);
            payload.put_u32(ioid, order);
            payload.put_u8(subcmd);
            synth_frame(Command::Get, order, payload)
        };

        // Last request: subcmd = 0x50 (0x40 | 0x10).
        let mut channels = make_channels();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let (exec_fin_tx, mut exec_fin_rx) = mpsc::unbounded_channel::<ExecFinished>();
        handle_op(
            &exec_frame(0x50),
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Get,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &exec_fin_tx,
        )
        .await
        .expect("GET EXEC (last request) ok");
        // The IOID stays reserved while the spawned reply task runs —
        // removal is deferred to the completion owner (pvxs cleanup() after
        // doReply), so a re-INIT racing the reply cannot reuse the IOID.
        {
            let op = channels
                .get(&sid)
                .unwrap()
                .ops
                .get(&ioid)
                .expect("last-request op stays reserved until its reply is sent");
            assert!(
                op.last_request,
                "the op is marked last_request so the completion owner frees it"
            );
        }
        // The response arrives — the task was not aborted by the deferral.
        let resp = rx.recv().await.expect("GET data response emitted");
        assert_eq!(
            resp[PvaHeader::SIZE + 4],
            0x50,
            "data response echoes the last-request subcmd"
        );
        assert!(
            resp.len() > PvaHeader::SIZE + 5,
            "data response carries a value, not a status-only frame"
        );
        // The completion signal fires once the reply task's guard drops;
        // applying it is where the last-request op's IOID is finally freed.
        let fin = exec_fin_rx
            .recv()
            .await
            .expect("data task signals completion");
        apply_exec_finish(&mut channels, fin);
        assert!(
            !channels.get(&sid).unwrap().ops.contains_key(&ioid),
            "last-request GET op is removed once its response has been sent"
        );

        // Not the last request: subcmd = 0x00. The op is returned to Idle on
        // completion, never removed.
        let mut channels = make_channels();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let (exec_fin_tx, mut exec_fin_rx) = mpsc::unbounded_channel::<ExecFinished>();
        handle_op(
            &exec_frame(0x00),
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Get,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &exec_fin_tx,
        )
        .await
        .expect("GET EXEC (not last request) ok");
        let _ = rx.recv().await.expect("GET data response emitted");
        let fin = exec_fin_rx
            .recv()
            .await
            .expect("data task signals completion");
        apply_exec_finish(&mut channels, fin);
        assert!(
            channels.get(&sid).unwrap().ops.contains_key(&ioid),
            "non-last-request GET EXEC keeps the op registered after completion"
        );
        assert_eq!(
            channels
                .get(&sid)
                .unwrap()
                .ops
                .get(&ioid)
                .unwrap()
                .exec_state,
            ExecState::Idle,
            "a completed non-last-request EXEC returns the op to Idle"
        );
    }

    /// A client may advertise an inbound descriptor once with a
    /// `0xFD <slot>` define and reference it later on the *same connection*
    /// with `0xFE <slot>` — pvxs keeps one connection-scope `rxRegistry`
    /// (conn.h:23) shared by every inbound decode. The native server's read
    /// loop owns one `rx_type_cache` threaded through every handler, so an
    /// INIT pvRequest descriptor defined by one operation resolves the
    /// reference made by a later operation. The negative control proves the
    /// shared cache is what closes the gap: a *fresh* cache (the pre-fix
    /// per-call behaviour) rejects the reference with a typecache miss.
    #[epics_macros_rs::epics_test]
    async fn init_pv_request_descriptor_resolves_cached_reference_across_ops() {
        use crate::pvdata::FieldDesc;
        use crate::pvdata::encode::{decode_type_desc_cached, encode_type_desc};
        use crate::server_native::SharedSource;
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::shared_pv::SharedPV;

        let order = ByteOrder::Little;
        let sid: u32 = 1;

        let intro = three_field_intro();
        let pv = SharedPV::new();
        pv.open(intro.clone(), three_field_value(0, 0, 0)).unwrap();
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
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // The "empty pvRequest" structure (no `field` child) → wildcard
        // mask. Its only role here is to be a real descriptor the client can
        // cache and reference.
        let req_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: Vec::new(),
        };
        const SLOT: u16 = 0x0001;

        // GET INIT #1 (ioid 801): pvRequest descriptor is a `0xFD <slot>`
        // define carrying the full inline descriptor.
        let ioid1: u32 = 801;
        let mut def_payload = Vec::new();
        def_payload.put_u32(sid, order);
        def_payload.put_u32(ioid1, order);
        def_payload.put_u8(0x08); // INIT
        def_payload.put_u8(0xFD);
        def_payload.put_u16(SLOT, order);
        encode_type_desc(&req_desc, order, &mut def_payload);
        let def_frame = synth_frame(Command::Get, order, def_payload);

        // GET INIT #2 (ioid 802): pvRequest descriptor is a bare `0xFE <slot>`
        // reference to the descriptor defined by INIT #1.
        let ioid2: u32 = 802;
        let mut ref_payload = Vec::new();
        ref_payload.put_u32(sid, order);
        ref_payload.put_u32(ioid2, order);
        ref_payload.put_u8(0x08); // INIT
        ref_payload.put_u8(0xFE);
        ref_payload.put_u16(SLOT, order);
        let ref_frame = synth_frame(Command::Get, order, ref_payload);

        // Shared connection cache: INIT #1 defines, INIT #2 resolves.
        let mut decode_cache = TypeCache::new();
        handle_op(
            &def_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Get,
            &config,
            &mut encode_cache,
            &mut decode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("GET INIT defining the pvRequest descriptor must succeed");
        let _ = rx.recv().await.expect("INIT #1 reply");
        assert!(
            decode_cache.contains_key(&SLOT),
            "INIT #1 must fold the 0xFD define into the connection cache"
        );

        handle_op(
            &ref_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Get,
            &config,
            &mut encode_cache,
            &mut decode_cache,
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("GET INIT referencing the cached pvRequest descriptor must succeed");
        let _ = rx.recv().await.expect("INIT #2 reply");
        assert!(
            channels.get(&sid).unwrap().ops.contains_key(&ioid2),
            "INIT #2's 0xFE reference resolved against the shared cache and registered the op"
        );

        // Negative control: a fresh per-call cache (pre-fix behaviour) cannot
        // resolve the reference — the INIT is rejected as connection-fatal.
        let mut empty_channels: HashMap<u32, ChannelState> = HashMap::new();
        empty_channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );
        let mut fresh_cache = TypeCache::new();
        let err = handle_op(
            &ref_frame,
            &tx,
            &mut empty_channels,
            order,
            &fixed_out_order(order),
            OpKind::Get,
            &config,
            &mut encode_cache,
            &mut fresh_cache,
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect_err("a 0xFE reference with no prior define must be a decode error");
        assert!(
            matches!(err, PvaError::Decode(_)),
            "unresolved type-cache reference is a connection-fatal decode error, got {err:?}"
        );

        // Sanity: the descriptor a fresh cache decodes from the *define*
        // frame matches the one stored from the connection cache, proving the
        // reference resolved to the same type.
        let mut probe = std::io::Cursor::new(&def_frame.payload[9..]);
        let defined = decode_type_desc_cached(&mut probe, order, &mut TypeCache::new())
            .expect("define frame descriptor decodes");
        assert_eq!(
            decode_cache.get(&SLOT),
            Some(&defined),
            "cached slot holds exactly the descriptor the define carried"
        );
    }

    /// RPC EXEC argument descriptors share the same connection cache as the
    /// pvRequest INIT descriptors (pvxs decodes both through the one
    /// `rxRegistry`). A define seen on an earlier frame must resolve a `0xFE`
    /// reference in a later RPC EXEC argument. `decode_rpc_exec_arg` is the
    /// exact decoder the RPC EXEC branch of `handle_op` invokes with the
    /// read-loop cache.
    #[test]
    fn rpc_exec_arg_resolves_cached_descriptor_reference() {
        use crate::pvdata::encode::{decode_type_desc_cached, encode_pv_field, encode_type_desc};
        use crate::pvdata::{FieldDesc, ScalarType, ScalarValue};

        let order = ByteOrder::Little;
        const SLOT: u16 = 0x0007;
        let arg_desc = FieldDesc::Scalar(ScalarType::Int);
        let arg_val = PvField::Scalar(ScalarValue::Int(42));

        // Frame 1: a `0xFD <slot>` define + value (a fully self-contained RPC
        // argument that also populates the cache).
        let mut def_buf = Vec::new();
        def_buf.put_u8(0xFD);
        def_buf.put_u16(SLOT, order);
        encode_type_desc(&arg_desc, order, &mut def_buf);
        encode_pv_field(&arg_val, &arg_desc, order, &mut def_buf);

        // Frame 2: a `0xFE <slot>` reference + value.
        let mut ref_buf = Vec::new();
        ref_buf.put_u8(0xFE);
        ref_buf.put_u16(SLOT, order);
        encode_pv_field(&arg_val, &arg_desc, order, &mut ref_buf);

        let mut cache = TypeCache::new();
        let mut cur1 = std::io::Cursor::new(def_buf.as_slice());
        let (d1, v1) =
            decode_rpc_exec_arg(&mut cur1, order, &mut cache).expect("define frame decodes");
        assert_eq!(d1, arg_desc);
        assert_eq!(v1, arg_val);

        let mut cur2 = std::io::Cursor::new(ref_buf.as_slice());
        let (d2, v2) = decode_rpc_exec_arg(&mut cur2, order, &mut cache)
            .expect("reference frame resolves against the shared cache");
        assert_eq!(d2, arg_desc, "0xFE resolved to the defined descriptor");
        assert_eq!(v2, arg_val);

        // Negative control: a fresh cache rejects the reference.
        let mut cur_fresh = std::io::Cursor::new(ref_buf.as_slice());
        decode_rpc_exec_arg(&mut cur_fresh, order, &mut TypeCache::new())
            .expect_err("a 0xFE reference with no prior define must be fatal");

        // Make the cross-frame intent explicit: pre-seeding only the cache
        // (no value bytes) is enough for the reference to resolve.
        let mut seed = TypeCache::new();
        let mut seed_cur = std::io::Cursor::new(&def_buf[..3 + 1]); // 0xFD + slot + 1-byte scalar code
        let _ = decode_type_desc_cached(&mut seed_cur, order, &mut seed).expect("seed define");
        let mut cur3 = std::io::Cursor::new(ref_buf.as_slice());
        let (d3, _) = decode_rpc_exec_arg(&mut cur3, order, &mut seed)
            .expect("reference resolves against a cache seeded by a prior define-only frame");
        assert_eq!(d3, arg_desc);
    }

    /// A PUT EXEC payload whose value embeds an `any` (Variant) field may
    /// carry the variant's element descriptor as a `0xFE <slot>` reference to
    /// a descriptor defined earlier on the connection.
    /// `decode_pv_field_with_bitset_cached` is the exact decoder the PUT EXEC
    /// branch of `handle_op` now invokes with the read-loop cache; it must
    /// resolve the embedded reference rather than fault the stream.
    #[test]
    fn put_exec_any_value_resolves_cached_variant_descriptor() {
        use crate::pvdata::encode::{
            decode_pv_field_with_bitset_cached, decode_type_desc_cached, encode_scalar_value,
            encode_type_desc,
        };
        use crate::pvdata::{FieldDesc, ScalarType, ScalarValue};

        let order = ByteOrder::Little;
        const SLOT: u16 = 0x0003;

        // Value type: a structure with a single `any` (Variant) leaf.
        // Bit numbering: root=0, `v`=1.
        let intro = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![("v".into(), FieldDesc::Variant)],
        };
        let inner_desc = FieldDesc::Scalar(ScalarType::Int);
        let inner_val = ScalarValue::Int(7);

        // Prior frame defines the variant's element descriptor in the cache.
        let mut def_buf = Vec::new();
        def_buf.put_u8(0xFD);
        def_buf.put_u16(SLOT, order);
        encode_type_desc(&inner_desc, order, &mut def_buf);
        let mut cache = TypeCache::new();
        let mut def_cur = std::io::Cursor::new(def_buf.as_slice());
        decode_type_desc_cached(&mut def_cur, order, &mut cache).expect("define inner descriptor");

        // PUT EXEC delta: changed bitset selects the `v` leaf (bit 1); the
        // variant body is `0xFE <slot>` + the inner scalar value.
        let mut changed = crate::proto::BitSet::new();
        changed.set(1);
        let mut delta_buf = Vec::new();
        delta_buf.put_u8(0xFE);
        delta_buf.put_u16(SLOT, order);
        encode_scalar_value(&inner_val, order, &mut delta_buf);

        let mut cur = std::io::Cursor::new(delta_buf.as_slice());
        let decoded =
            decode_pv_field_with_bitset_cached(&intro, &changed, 0, &mut cur, order, &mut cache)
                .expect("PUT EXEC any value with a cached variant descriptor decodes");
        let PvField::Structure(s) = decoded else {
            panic!("expected a structure, got {decoded:?}");
        };
        let (_, v) = s
            .fields
            .iter()
            .find(|(n, _)| n == "v")
            .expect("field v present");
        // The `any` leaf carries its resolved element descriptor + value; the
        // 0xFE reference must have resolved to the cached `Int` descriptor.
        let PvField::Variant(var) = v else {
            panic!("expected an `any`/Variant leaf, got {v:?}");
        };
        assert_eq!(
            var.desc.as_ref(),
            Some(&inner_desc),
            "the 0xFE reference resolved to the cached Int descriptor"
        );
        assert_eq!(
            var.value,
            PvField::Scalar(ScalarValue::Int(7)),
            "the variant value decoded against the resolved descriptor"
        );

        // Negative control: a fresh cache cannot resolve the reference.
        let mut cur_fresh = std::io::Cursor::new(delta_buf.as_slice());
        decode_pv_field_with_bitset_cached(
            &intro,
            &changed,
            0,
            &mut cur_fresh,
            order,
            &mut TypeCache::new(),
        )
        .expect_err("a 0xFE variant reference with no prior define must fault");
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

    /// Regression: the default `ChannelSource::put_delta_checked`
    /// merges the sparse delta over the PV's prior value. The
    /// prior-value read MUST run through `get_value_checked` (the
    /// credentialed path) — not the ctx-less `get_value` — so an
    /// access-controlled or credential-routed source resolves the
    /// prior under the same identity as the write.
    ///
    /// This test source returns a WRONG prior `(0,0,0)` from the
    /// ctx-less `get_value` and the CORRECT prior `(10,20,30)` from
    /// the credentialed `get_value_checked`. A single-field delta PUT
    /// marking only `b` must produce `(10,99,30)`. Before the fix the
    /// default read the prior through `get_value`, so `a`/`c`
    /// collapsed to `0` (the wrong-context value).
    #[epics_macros_rs::epics_test]
    async fn ex_r3_put_delta_default_merges_under_credentialed_read() {
        use crate::proto::BitSet;
        use crate::server_native::source::{
            AccessChecked, AccessGate, ChannelContext, ChannelSource,
        };
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        // Source whose ctx-less `get_value` deliberately returns a
        // different (wrong) prior than the credentialed
        // `get_value_checked`. Records which read path the default
        // `put_delta_checked` exercised, and stores the final value.
        struct SplitReadSource {
            stored: Arc<parking_lot::Mutex<Option<PvField>>>,
            used_ctxless_get: Arc<AtomicBool>,
        }
        impl ChannelSource for SplitReadSource {
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
            // Ctx-less read — the WRONG prior. If the default
            // put_delta_checked uses this, the merge loses a/c.
            fn get_value(
                &self,
                _: &str,
            ) -> impl std::future::Future<Output = Option<PvField>> + Send {
                self.used_ctxless_get.store(true, Ordering::SeqCst);
                async { Some(three_field_value(0, 0, 0)) }
            }
            // Credentialed read — the CORRECT prior.
            async fn get_value_checked(
                &self,
                checked: AccessChecked,
                _ctx: ChannelContext,
            ) -> Option<PvField> {
                if !checked.allows_read() {
                    return None;
                }
                Some(three_field_value(10, 20, 30))
            }
            fn put_value(
                &self,
                _: &str,
                value: PvField,
            ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
                *self.stored.lock() = Some(value);
                async { Ok(()) }
            }
            async fn is_writable(&self, _: &str) -> bool {
                true
            }
            async fn subscribe(&self, _: &str) -> Option<MonitorStream<PvField>> {
                None
            }
        }

        let stored = Arc::new(parking_lot::Mutex::new(None));
        let used_ctxless = Arc::new(AtomicBool::new(false));
        let src = SplitReadSource {
            stored: stored.clone(),
            used_ctxless_get: used_ctxless.clone(),
        };

        // ReadWrite token from the default Open gate.
        let checked = AccessGate::open()
            .check("dut", "h", "u", "anonymous", "")
            .await;
        let ctx = ChannelContext {
            peer: "127.0.0.1:5075".parse().unwrap(),
            account: "u".into(),
            method: "anonymous".into(),
            host: "h".into(),
            authority: String::new(),
            roles: Vec::new(),
            pv_request: None,
            log: Default::default(),
        };

        // Delta marking only field `b` (bit 2) -> 99.
        let mut changed = BitSet::new();
        changed.set(2);
        let delta = three_field_value(0, 99, 0);

        src.put_delta_checked(
            checked,
            std::sync::Arc::new(three_field_intro()),
            changed,
            delta,
            ctx,
        )
        .await
        .expect("put_delta_checked must succeed");

        let final_value = stored.lock().clone().expect("a value must be stored");
        let (a, b, c) = three_field_extract(&final_value);
        assert_eq!(
            (a, b, c),
            (10, 99, 30),
            "default put_delta_checked must merge the delta over the \
             credentialed prior (10,20,30); got ({a},{b},{c})"
        );
        assert!(
            !used_ctxless.load(Ordering::SeqCst),
            "the prior-value read must NOT go through the ctx-less get_value"
        );
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
    #[epics_macros_rs::epics_test]
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
        let pv = SharedPV::build_mailbox();
        pv.open(intro.clone(), three_field_value(10, 20, 30))
            .unwrap();

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
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

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
            &init_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Put,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
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
            &exec_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Put,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("PUT EXEC ok");
        let _ = rx.recv().await.expect("PUT EXEC response emitted");

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
    #[epics_macros_rs::epics_test]
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
        let pv = SharedPV::build_mailbox();
        pv.open(intro.clone(), three_field_value(10, 20, 30))
            .unwrap();

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
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

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
            &init_frame,
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_exec_fin(),
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
            &data_frame,
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_exec_fin(),
        )
        .await
        .expect("PUT_GET data ok");
        let resp = rx.recv().await.expect("PUT_GET data response emitted");

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
            // Writable PV: a plain SharedPV is not writable (pvxs
            // parity), so the atomic-merge PUT path needs a mailbox.
            let pv = SharedPV::build_mailbox();
            pv.open(intro.clone(), three_field_value(0, 0, 0)).unwrap();
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
                roles: Vec::new(),
                pv_request: None,
                log: Default::default(),
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
                    .put_delta_checked(
                        checked,
                        std::sync::Arc::new(intro_a),
                        changed_a,
                        delta_a,
                        ctx_a,
                    )
                    .await
            });
            let task_c = tokio::spawn(async move {
                let checked = src_c
                    .access()
                    .check("dut", &ctx_c.host, &ctx_c.account, &ctx_c.method, "")
                    .await;
                src_c
                    .put_delta_checked(
                        checked,
                        std::sync::Arc::new(intro_c),
                        changed_c,
                        delta_c,
                        ctx_c,
                    )
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
            let cell = epics_base_rs::server::access_security::new_acf_cell(Some(acf));
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
        ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
            let (a, _, _) = three_field_extract(&value);
            *self.value.lock() = a;
            async { Ok(()) }
        }
        async fn is_writable(&self, _: &str) -> bool {
            true
        }
        async fn subscribe(&self, _: &str) -> Option<MonitorStream<PvField>> {
            None
        }
        fn process(
            &self,
            _: &str,
        ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
            self.process_hits
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { Ok(()) }
        }
    }

    /// Build the (sid → ChannelState) map plus a primed PROCESS op
    /// for `ioid`, so a PROCESS data-phase frame dispatches straight
    /// into the WRITE-gate check.
    #[cfg(test)]
    fn primed_process_channels(
        sid: u32,
        ioid: u32,
        source: DynSource,
    ) -> HashMap<u32, ChannelState> {
        let intro = three_field_intro();
        let mut ops = HashMap::new();
        let mask = BitSet::all_set(intro.total_bits());
        ops.insert(
            ioid,
            non_monitor_op_state(std::sync::Arc::new(intro.clone()), OpKind::Process, mask),
        );
        let mut channels = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(intro)),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
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

    /// A1: the ACF host identity must come from the socket, never the wire.
    ///
    /// A crafted `ca` client sends `host` naming a machine an operator's
    /// `HAG(...)` trusts. A2 is the same defect through `asCheckClientIP=1`,
    /// where `hag_members` (`access_security.rs:1499-1529`) stores either a
    /// dotted quad or the literal `unresolved:<name>` sentinel a failed
    /// load-time DNS lookup leaves behind — so both of those are typeable
    /// too, and the sentinel turns a *failed* lookup into a password. All
    /// three shapes are covered below. Pre-fix the string was copied
    /// verbatim into `ClientCredentials::host`
    /// (`tcp.rs:2768`), which is the value `compute_rules` matches HAG
    /// members against, so a client could grant itself any host-scoped rule.
    ///
    /// pvxs cannot express this: `server::ClientCredentials`
    /// (`src/pvxs/srvcommon.h:36-56`) has no host field, and QSRV derives it
    /// from the socket (`ioc/credentials.cpp:27-29`).
    ///
    /// Fails today on Linux with no network: it is a pure decode.
    #[test]
    fn parse_client_credentials_never_takes_the_acf_host_from_the_wire() {
        let order = ByteOrder::Little;
        let account = "pvxs_nobody_zz";
        // Both shapes an attacker would pick: a trusted-looking name, and
        // the sentinel a failed HAG lookup leaves behind (A2).
        for forged in ["trusted-console.lab", "unresolved:lab-pc1", "192.0.2.7"] {
            let mut payload = Vec::new();
            payload.put_u32(0x10000, order); // buffer_size
            payload.put_u16(1, order); // intro_size
            payload.put_u16(0, order); // qos
            encode_string_into("ca", order, &mut payload);
            payload.put_u8(0xFD);
            payload.put_u16(1, order);
            payload.put_u8(0x80);
            payload.put_u8(0x00);
            payload.put_u8(2); // n_fields
            payload.put_u8(0x04);
            payload.extend_from_slice(b"user");
            payload.put_u8(0x60); // string
            payload.put_u8(0x04);
            payload.extend_from_slice(b"host");
            payload.put_u8(0x60); // string
            encode_string_into(account, order, &mut payload);
            encode_string_into(forged, order, &mut payload);

            let header = PvaHeader::application(
                false,
                order,
                Command::ConnectionValidation.code(),
                payload.len() as u32,
            );
            let frame = Frame { header, payload };

            let creds = parse_client_credentials(&frame, &mut TypeCache::new(), TEST_PEER)
                .expect("decode must succeed")
                .expect("ca with a user field yields Some");

            assert_ne!(
                creds.host, forged,
                "the wire `host` reached the field the HAG gate matches"
            );
            assert_eq!(
                creds.host, "198.51.100.7",
                "host must be the numeric peer (QSRV credentials.cpp:27-29)"
            );
        }
    }

    /// The peer-derivation itself, on the boundaries QSRV's `map6to4` covers.
    #[test]
    fn acf_host_from_peer_matches_qsrv_map6to4() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

        let v4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3)), 5075);
        assert_eq!(ClientCredentials::acf_host_from_peer(v4), "10.0.0.3");

        // IPv4-mapped IPv6 renders as its IPv4 form, so a dual-stack peer
        // matches the same HAG entry it would over IPv4.
        let mapped = SocketAddr::new(
            IpAddr::V6(Ipv4Addr::new(10, 0, 0, 3).to_ipv6_mapped()),
            5075,
        );
        assert_eq!(ClientCredentials::acf_host_from_peer(mapped), "10.0.0.3");

        // A genuine IPv6 peer keeps its own form; `to_ipv4` (as opposed to
        // `to_ipv4_mapped`) would have rendered ::1 as "0.0.0.1".
        let v6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 5075);
        assert_eq!(ClientCredentials::acf_host_from_peer(v6), "::1");
    }

    /// Security regression: the server MUST NOT trust wire-advertised
    /// `groups`/`roles`. A crafted `ca` client sends `user=<unknown acct>`
    /// plus `groups=["admin","wheel"]`; `parse_client_credentials` must
    /// ignore the wire groups and re-derive roles server-side from the
    /// account (pvxs `ClientCredentials::roles()` → `osdGetRoles`). For an
    /// account unknown to the local passwd DB that derivation yields
    /// `{account}` — never the attacker's claim. Pre-fix the wire array
    /// landed directly in `creds.roles`, so `member group:admin` matched.
    #[test]
    fn parse_client_credentials_ignores_wire_advertised_roles() {
        let order = ByteOrder::Little;
        // Unknown to any local passwd DB → osd_get_roles is the
        // deterministic {account} fallback.
        let fake_account = "pvxs_nobody_zz";
        let wire_groups = ["admin", "wheel"];

        // Mirror `build_client_connection_validation`'s inline AuthZ
        // structure: user(str) + host(str) + groups(str[]).
        let mut payload = Vec::new();
        payload.put_u32(0x10000, order); // buffer_size
        payload.put_u16(1, order); // intro_size
        payload.put_u16(0, order); // qos
        encode_string_into("ca", order, &mut payload);
        payload.put_u8(0xFD);
        payload.put_u16(1, order);
        payload.put_u8(0x80);
        payload.put_u8(0x00);
        payload.put_u8(3); // n_fields
        payload.put_u8(0x04);
        payload.extend_from_slice(b"user");
        payload.put_u8(0x60); // string
        payload.put_u8(0x04);
        payload.extend_from_slice(b"host");
        payload.put_u8(0x60); // string
        payload.put_u8(0x06);
        payload.extend_from_slice(b"groups");
        payload.put_u8(0x68); // string[]
        encode_string_into(fake_account, order, &mut payload);
        encode_string_into("h.example", order, &mut payload);
        crate::proto::encode_size_into(wire_groups.len() as u32, order, &mut payload);
        for g in wire_groups {
            encode_string_into(g, order, &mut payload);
        }

        let header = PvaHeader::application(
            false,
            order,
            Command::ConnectionValidation.code(),
            payload.len() as u32,
        );
        let frame = Frame { header, payload };

        let creds = parse_client_credentials(&frame, &mut TypeCache::new(), TEST_PEER)
            .expect("decode must succeed")
            .expect("ca with a user field yields Some");

        assert_eq!(
            creds.account, fake_account,
            "account comes from wire `user`"
        );
        assert!(
            !creds.roles.iter().any(|r| r == "admin" || r == "wheel"),
            "wire-advertised roles leaked into the credential: {:?}",
            creds.roles
        );
        assert_eq!(
            creds.roles,
            crate::auth::osd_get_roles(fake_account),
            "roles must be re-derived server-side from the account"
        );
    }

    /// A `ca` CONNECTION_VALIDATION with a NULL (`0xFF`) auth value carries
    /// no `user` field, so pvxs leaves the credential at the anonymous
    /// placeholder (`serverconn.cpp:223-231` only sets account inside the
    /// `auth["user"]` callback). The pre-fix null-auth branch returned early
    /// with `method="ca", account=""` — a CA identity pvxs never produces.
    /// It must now take the same anonymous fallback as a no-user structure.
    #[test]
    fn parse_client_credentials_ca_null_auth_falls_back_to_anonymous() {
        let order = ByteOrder::Little;
        let mut payload = Vec::new();
        payload.put_u32(0x10000, order); // buffer_size
        payload.put_u16(1, order); // intro_size
        payload.put_u16(0, order); // qos
        encode_string_into("ca", order, &mut payload);
        payload.put_u8(0xFF); // NULL auth Value (no user/host fields)

        let header = PvaHeader::application(
            false,
            order,
            Command::ConnectionValidation.code(),
            payload.len() as u32,
        );
        let frame = Frame { header, payload };

        let parsed = parse_client_credentials(&frame, &mut TypeCache::new(), TEST_PEER)
            .expect("decode must succeed");
        assert!(
            parsed.is_none(),
            "ca + null auth must yield the anonymous fallback (None), \
             not a blank-account CA credential: {parsed:?}"
        );
    }

    /// pvxs advertises exactly `anonymous`/`ca` (`serverconn.cpp:108-114`)
    /// and keys the auth lambda on the raw string `selected=="ca"`
    /// (`serverconn.cpp:221-231`). An uppercase `CA` is therefore NOT the
    /// advertised method: the lambda never fires to fold it into a trusted
    /// ca identity, the parser returns the spelling verbatim, and the caller
    /// rejects it as unadvertised (`serverconn.cpp:238-241`). A
    /// case-insensitive compare would fold `CA`+user into a clean ca
    /// credential the client never legitimately negotiated.
    #[test]
    fn parse_client_credentials_uppercase_ca_is_not_advertised_method() {
        let order = ByteOrder::Little;
        let mut payload = Vec::new();
        payload.put_u32(0x10000, order); // buffer_size
        payload.put_u16(1, order); // intro_size
        payload.put_u16(0, order); // qos
        encode_string_into("CA", order, &mut payload);
        // AuthZ structure carrying a single `user` field.
        payload.put_u8(0xFD);
        payload.put_u16(1, order);
        payload.put_u8(0x80);
        payload.put_u8(0x00);
        payload.put_u8(1); // n_fields
        payload.put_u8(0x04);
        payload.extend_from_slice(b"user");
        payload.put_u8(0x60); // string
        encode_string_into("alice", order, &mut payload);

        let header = PvaHeader::application(
            false,
            order,
            Command::ConnectionValidation.code(),
            payload.len() as u32,
        );
        let frame = Frame { header, payload };

        let creds = parse_client_credentials(&frame, &mut TypeCache::new(), TEST_PEER)
            .expect("decode must succeed")
            .expect("non-anonymous method yields Some");
        assert_eq!(
            creds.method, "CA",
            "method preserved byte-for-byte, never folded to `ca`"
        );
        assert!(
            !["anonymous", "ca"].iter().any(|m| *m == creds.method),
            "uppercase `CA` must not match an advertised method: {:?}",
            creds.method
        );
    }

    /// A mixed-case `Ca` with no user field must NOT take the
    /// ca-requires-user anonymous fallback: that fallback keys on the exact
    /// string `ca` (`serverconn.cpp:221-231`), so `Ca` falls through with
    /// its verbatim method and is rejected by the caller as unadvertised
    /// (`serverconn.cpp:238-241`) rather than silently treated as a clean
    /// anonymous handshake.
    #[test]
    fn parse_client_credentials_mixedcase_ca_missing_user_is_not_advertised() {
        let order = ByteOrder::Little;
        let mut payload = Vec::new();
        payload.put_u32(0x10000, order); // buffer_size
        payload.put_u16(1, order); // intro_size
        payload.put_u16(0, order); // qos
        encode_string_into("Ca", order, &mut payload);
        payload.put_u8(0xFF); // NULL auth value (no user field)

        let header = PvaHeader::application(
            false,
            order,
            Command::ConnectionValidation.code(),
            payload.len() as u32,
        );
        let frame = Frame { header, payload };

        let creds = parse_client_credentials(&frame, &mut TypeCache::new(), TEST_PEER)
            .expect("decode must succeed")
            .expect("mixed-case `Ca` is not the exact `ca` fallback → Some");
        assert_eq!(
            creds.method, "Ca",
            "method preserved byte-for-byte, never folded to `ca`"
        );
        assert!(
            !["anonymous", "ca"].iter().any(|m| *m == creds.method),
            "mixed-case `Ca` must not match an advertised method: {:?}",
            creds.method
        );
    }

    /// A capitalized `Anonymous` is not the advertised `anonymous`
    /// (`serverconn.cpp:108-114`). Only the byte-exact `anonymous` (or an
    /// empty method) folds to the anonymous placeholder; `Anonymous` falls
    /// through verbatim and is rejected as unadvertised
    /// (`serverconn.cpp:238-241`), never case-folded into a clean anonymous
    /// handshake.
    #[test]
    fn parse_client_credentials_capitalized_anonymous_is_not_folded() {
        let order = ByteOrder::Little;
        let mut payload = Vec::new();
        payload.put_u32(0x10000, order); // buffer_size
        payload.put_u16(1, order); // intro_size
        payload.put_u16(0, order); // qos
        encode_string_into("Anonymous", order, &mut payload);
        payload.put_u8(0xFF); // NULL auth value

        let header = PvaHeader::application(
            false,
            order,
            Command::ConnectionValidation.code(),
            payload.len() as u32,
        );
        let frame = Frame { header, payload };

        let creds = parse_client_credentials(&frame, &mut TypeCache::new(), TEST_PEER)
            .expect("decode must succeed")
            .expect("capitalized `Anonymous` is not the exact fold → Some");
        assert_eq!(
            creds.method, "Anonymous",
            "method preserved byte-for-byte, never folded to `anonymous`"
        );
        assert!(
            !["anonymous", "ca"].iter().any(|m| *m == creds.method),
            "capitalized `Anonymous` must not match an advertised method: {:?}",
            creds.method
        );
    }

    /// Regression (Defect 1, native PROCESS handler): a peer whose
    /// x509 `authority` matches an `AUTHORITY(...)`-scoped WRITE rule
    /// MUST be granted PROCESS. `handle_process` passed a literal
    /// `""` as the authority to `AccessGate::check`, so the
    /// matching-CA peer failed `authority_match` and was wrongly
    /// denied — its `process` hook never ran.
    #[epics_macros_rs::epics_test]
    async fn process_honors_authority_scoped_write_rule() {
        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 500;
        let src = AuthorityGatedSource::new();
        let process_hits = std::sync::Arc::clone(&src.process_hits);
        let source: DynSource = std::sync::Arc::new(src);

        let mut channels = primed_process_channels(sid, ioid, source.clone());
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
            &frame,
            &tx,
            &mut channels,
            order,
            &config,
            &mut TypeCache::new(),
            peer,
            &x509_cred("MyCA"),
            &discard_exec_fin(),
        )
        .await
        .expect("handle_process ok");

        let resp = rx.recv().await.expect("PROCESS response emitted");
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
    #[epics_macros_rs::epics_test]
    async fn process_denied_for_wrong_authority() {
        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 501;
        let src = AuthorityGatedSource::new();
        let process_hits = std::sync::Arc::clone(&src.process_hits);
        let source: DynSource = std::sync::Arc::new(src);

        let mut channels = primed_process_channels(sid, ioid, source.clone());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();

        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x00);
        let frame = synth_frame(Command::Process, order, payload);

        handle_process(
            &frame,
            &tx,
            &mut channels,
            order,
            &config,
            &mut TypeCache::new(),
            peer,
            &x509_cred("OtherCA"),
            &discard_exec_fin(),
        )
        .await
        .expect("handle_process ok");

        let resp = rx.recv().await.expect("PROCESS response emitted");
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

    /// Build a channel whose `ioid` op was initialised as `kind`,
    /// so a wrong-kind data frame can be driven against it.
    #[cfg(test)]
    fn primed_channels_with_kind(
        sid: u32,
        ioid: u32,
        kind: OpKind,
        source: DynSource,
    ) -> HashMap<u32, ChannelState> {
        let intro = three_field_intro();
        let mask = BitSet::all_set(intro.total_bits());
        let mut ops = HashMap::new();
        ops.insert(
            ioid,
            non_monitor_op_state(std::sync::Arc::new(intro.clone()), kind, mask),
        );
        let mut channels = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(intro)),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops,
            },
        );
        channels
    }

    /// Regression: the dedicated `handle_process` data phase must
    /// reject a frame whose IOID was initialised as a different
    /// operation class. Before the fix it only checked
    /// `ch.ops.contains_key(ioid)`, so a client could INIT a GET (or
    /// MONITOR) and then drive a PROCESS data frame through it,
    /// triggering record processing on an op that never negotiated
    /// PROCESS. pvxs `serverget.cpp:421-436` resets the connection on
    /// a wrong-kind IOID.
    #[epics_macros_rs::epics_test]
    async fn ex_r5_process_data_rejects_get_initialised_ioid() {
        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 600;
        let src = AuthorityGatedSource::new();
        let process_hits = std::sync::Arc::clone(&src.process_hits);
        let source: DynSource = std::sync::Arc::new(src);

        // IOID initialised as a GET, not a PROCESS.
        let mut channels = primed_channels_with_kind(sid, ioid, OpKind::Get, source.clone());
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();

        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x00);
        let frame = synth_frame(Command::Process, order, payload);

        let res = handle_process(
            &frame,
            &tx,
            &mut channels,
            order,
            &config,
            &mut TypeCache::new(),
            peer,
            &x509_cred("MyCA"),
            &discard_exec_fin(),
        )
        .await;
        assert!(
            res.is_err(),
            "PROCESS data against a GET-initialised IOID must be a protocol error"
        );
        assert_eq!(
            process_hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "record processing must NOT run for a wrong-kind PROCESS frame"
        );
    }

    /// Regression: `handle_process` must also reject a PROCESS
    /// data frame against a MONITOR-initialised IOID.
    #[epics_macros_rs::epics_test]
    async fn ex_r5_process_data_rejects_monitor_initialised_ioid() {
        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 601;
        let src = AuthorityGatedSource::new();
        let process_hits = std::sync::Arc::clone(&src.process_hits);
        let source: DynSource = std::sync::Arc::new(src);

        let mut channels = primed_channels_with_kind(sid, ioid, OpKind::Monitor, source.clone());
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();

        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x00);
        let frame = synth_frame(Command::Process, order, payload);

        let res = handle_process(
            &frame,
            &tx,
            &mut channels,
            order,
            &config,
            &mut TypeCache::new(),
            peer,
            &x509_cred("MyCA"),
            &discard_exec_fin(),
        )
        .await;
        assert!(
            res.is_err(),
            "PROCESS data against a MONITOR-initialised IOID must be a protocol error"
        );
        assert_eq!(
            process_hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "record processing must NOT run for a wrong-kind PROCESS frame"
        );
    }

    /// a (sid → ChannelState) map with introspection but NO
    /// registered ops, so a PROCESS INIT frame exercises the INIT
    /// pvRequest decode + registration path.
    #[cfg(test)]
    fn process_channels_no_op(sid: u32, source: DynSource) -> HashMap<u32, ChannelState> {
        let intro = three_field_intro();
        let mut channels = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(intro)),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );
        channels
    }

    /// build a PROCESS INIT frame `sid + ioid + 0x08 +
    /// pv_request_bytes`.
    #[cfg(test)]
    fn process_init_frame(sid: u32, ioid: u32, pv_request: &[u8], order: ByteOrder) -> Frame {
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x08); // INIT
        payload.extend_from_slice(pv_request);
        synth_frame(Command::Process, order, payload)
    }

    /// drive `handle_process` for an INIT frame and return
    /// `(connection_fatal, ops contains ioid, reply status success)`.
    /// `reply_success` is `None` when no op reply was emitted (the
    /// connection-fatal `bev.reset()` path emits no frame).
    #[cfg(test)]
    async fn run_process_init(
        sid: u32,
        ioid: u32,
        pv_request: &[u8],
        order: ByteOrder,
    ) -> (bool, bool, Option<bool>) {
        let source: DynSource = std::sync::Arc::new(AuthorityGatedSource::new());
        let mut channels = process_channels_no_op(sid, source.clone());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let peer: SocketAddr = "127.0.0.1:5076".parse().unwrap();
        let frame = process_init_frame(sid, ioid, pv_request, order);

        let result = handle_process(
            &frame,
            &tx,
            &mut channels,
            order,
            &config,
            &mut TypeCache::new(),
            peer,
            &x509_cred("MyCA"),
            &discard_exec_fin(),
        )
        .await;
        let fatal = result.is_err();

        let registered = channels.get(&sid).unwrap().ops.contains_key(&ioid);
        let reply_success = rx.try_recv().ok().map(|resp| {
            let (rframe, _) = try_parse_frame(&resp)
                .expect("frame parses")
                .expect("complete frame");
            let mut cur = rframe.cursor();
            let _ioid = cur.get_u32(order).expect("ioid");
            let _subcmd = cur.get_u8().expect("subcmd");
            Status::decode(&mut cur, order)
                .expect("status")
                .is_success()
        });
        (fatal, registered, reply_success)
    }

    /// Regression: a PROCESS INIT whose pvRequest VALUE is present but
    /// truncated is a peer wire-decode fault — connection-fatal
    /// (`bev.reset()` parity, pvxs `serverget.cpp:371-375`), emitting no
    /// op reply and registering no IOID. The previous
    /// `decode_type_desc(..).ok().and_then(|d| decode_pv_field(..).ok())`
    /// swallowed the value error and registered the op with `Status::ok()`.
    #[epics_macros_rs::epics_test]
    async fn bfr8_process_init_truncated_value_rejected_and_unregistered() {
        let order = ByteOrder::Little;
        // Valid Int descriptor, then a truncated (2-byte) i32 value.
        let desc = FieldDesc::Scalar(crate::pvdata::ScalarType::Int);
        let mut req = Vec::new();
        crate::pvdata::encode::encode_type_desc(&desc, order, &mut req);
        req.extend_from_slice(&[1u8, 2u8]);

        let (fatal, registered, reply) = run_process_init(1, 700, &req, order).await;
        assert!(
            fatal,
            "a malformed PROCESS INIT pvRequest value must be connection-fatal"
        );
        assert!(
            !registered,
            "a malformed PROCESS INIT pvRequest value must not register the IOID"
        );
        assert!(reply.is_none(), "the bev.reset() path emits no op reply");
    }

    /// Regression: a PROCESS INIT with no decodable pvRequest descriptor
    /// (empty body after subcmd) is a peer wire-decode fault —
    /// connection-fatal, not registered, no reply (pvxs faults the buffer
    /// and `bev.reset()`s).
    #[epics_macros_rs::epics_test]
    async fn bfr8_process_init_missing_descriptor_rejected_and_unregistered() {
        let order = ByteOrder::Little;
        let (fatal, registered, reply) = run_process_init(1, 701, &[], order).await;
        assert!(
            fatal,
            "a PROCESS INIT with no pvRequest descriptor must be connection-fatal"
        );
        assert!(
            !registered,
            "a PROCESS INIT with no pvRequest descriptor must not register the IOID"
        );
        assert!(reply.is_none(), "the bev.reset() path emits no op reply");
    }

    /// Control: a well-formed PROCESS INIT pvRequest (the Rust
    /// client's shape — descriptor + value) is accepted, registers the
    /// IOID, and replies `Status::ok()`. Guards against the rejection
    /// path over-firing on valid input.
    #[epics_macros_rs::epics_test]
    async fn bfr8_process_init_valid_pvrequest_registers_op() {
        let order = ByteOrder::Little;
        let desc = FieldDesc::Scalar(crate::pvdata::ScalarType::Int);
        let mut req = Vec::new();
        crate::pvdata::encode::encode_type_desc(&desc, order, &mut req);
        crate::pvdata::encode::encode_pv_field(
            &PvField::Scalar(crate::pvdata::ScalarValue::Int(7)),
            &desc,
            order,
            &mut req,
        );

        let (fatal, registered, reply) = run_process_init(1, 702, &req, order).await;
        assert!(!fatal, "a valid PROCESS INIT must not be connection-fatal");
        assert!(registered, "a valid PROCESS INIT must register the IOID");
        assert_eq!(
            reply,
            Some(true),
            "a valid PROCESS INIT must reply Status::ok()"
        );
    }

    /// Regression: a PROCESS INIT carrying a non-null descriptor that needs
    /// value bytes (scalar Int) but ENDS before them — a descriptor-only
    /// frame — is the `from_wire_type_value` -> `!M.good()` wire fault
    /// (`dataencode.cpp:747-752`, `serverget.cpp:371-375`): connection-fatal,
    /// not registered, no reply. The prior cursor-exhausted short-circuit to
    /// `Ok(None)` accepted it and registered the op with `Status::ok()`.
    #[epics_macros_rs::epics_test]
    async fn process_init_descriptor_only_value_rejected_and_unregistered() {
        let order = ByteOrder::Little;
        // Scalar Int descriptor, no value bytes.
        let desc = FieldDesc::Scalar(crate::pvdata::ScalarType::Int);
        let mut req = Vec::new();
        crate::pvdata::encode::encode_type_desc(&desc, order, &mut req);

        let (fatal, registered, reply) = run_process_init(1, 703, &req, order).await;
        assert!(
            fatal,
            "a descriptor-only PROCESS INIT (value bytes absent) must be connection-fatal"
        );
        assert!(
            !registered,
            "a descriptor-only PROCESS INIT must not register the IOID"
        );
        assert!(reply.is_none(), "the bev.reset() path emits no op reply");
    }

    /// build a ChannelArray INIT frame `sid + ioid + 0x08 +
    /// pv_request_bytes`.
    #[cfg(test)]
    fn array_init_frame(sid: u32, ioid: u32, pv_request: &[u8], order: ByteOrder) -> Frame {
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x08); // INIT
        payload.extend_from_slice(pv_request);
        synth_frame(Command::Array, order, payload)
    }

    /// drive `handle_channel_array` for an INIT frame and return
    /// `(connection_fatal, ops contains ioid, an op reply was emitted)`.
    #[cfg(test)]
    async fn run_array_init(
        sid: u32,
        ioid: u32,
        pv_request: &[u8],
        order: ByteOrder,
    ) -> (bool, bool, bool) {
        let source: DynSource = std::sync::Arc::new(AuthorityGatedSource::new());
        let mut channels = process_channels_no_op(sid, source.clone());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5076".parse().unwrap();
        let frame = array_init_frame(sid, ioid, pv_request, order);

        let result = handle_channel_array(
            &frame,
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &x509_cred("MyCA"),
            &discard_exec_fin(),
        )
        .await;
        let fatal = result.is_err();
        let registered = channels.get(&sid).unwrap().ops.contains_key(&ioid);
        let replied = rx.try_recv().is_ok();
        (fatal, registered, replied)
    }

    /// Regression: a ChannelArray INIT carrying a non-null descriptor that
    /// needs value bytes (scalar Int) but ENDS before them is the
    /// `from_wire_type_value` wire fault — connection-fatal, not registered,
    /// no reply. The decode runs before `channel_array_init`, so the fault
    /// fires at the wire boundary exactly as pvxs `bev.reset()` does.
    #[epics_macros_rs::epics_test]
    async fn array_init_descriptor_only_value_rejected_and_unregistered() {
        let order = ByteOrder::Little;
        // Scalar Int descriptor, no value bytes.
        let desc = FieldDesc::Scalar(crate::pvdata::ScalarType::Int);
        let mut req = Vec::new();
        crate::pvdata::encode::encode_type_desc(&desc, order, &mut req);

        let (fatal, registered, replied) = run_array_init(1, 704, &req, order).await;
        assert!(
            fatal,
            "a descriptor-only ARRAY INIT (value bytes absent) must be connection-fatal"
        );
        assert!(
            !registered,
            "a descriptor-only ARRAY INIT must not register the IOID"
        );
        assert!(!replied, "the bev.reset() path emits no op reply");
    }

    /// Regression: the dedicated `handle_put_get` data phase must
    /// reject a frame whose IOID was initialised as a different
    /// operation class (here a GET). Before the fix it extracted
    /// `(intro, mask)` from whatever op existed and performed a
    /// write/readback the operation never negotiated as a PUT_GET.
    #[epics_macros_rs::epics_test]
    async fn ex_r5_put_get_data_rejects_get_initialised_ioid() {
        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 602;
        let source: DynSource = std::sync::Arc::new(AuthorityGatedSource::new());

        let mut channels = primed_channels_with_kind(sid, ioid, OpKind::Get, source.clone());
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let intro = three_field_intro();

        // PUT_GET data frame: sid + ioid + subcmd(0x00) + bitset + value.
        let mut changed = BitSet::new();
        changed.set(2);
        let delta = three_field_value(0, 99, 0);
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x00);
        changed.write_into(order, &mut payload);
        crate::pvdata::encode::encode_pv_field_with_bitset(
            &delta,
            &intro,
            &changed,
            0,
            order,
            &mut payload,
        );
        let frame = synth_frame(Command::PutGet, order, payload);

        let res = handle_put_get(
            &frame,
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &x509_cred("MyCA"),
            &discard_exec_fin(),
        )
        .await;
        assert!(
            res.is_err(),
            "PUT_GET data against a GET-initialised IOID must be a protocol error"
        );
    }

    /// Regression: `handle_put_get` must also reject a PUT_GET
    /// data frame against a PUT-initialised IOID.
    #[epics_macros_rs::epics_test]
    async fn ex_r5_put_get_data_rejects_put_initialised_ioid() {
        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 603;
        let source: DynSource = std::sync::Arc::new(AuthorityGatedSource::new());

        let mut channels = primed_channels_with_kind(sid, ioid, OpKind::Put, source.clone());
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let intro = three_field_intro();

        let mut changed = BitSet::new();
        changed.set(2);
        let delta = three_field_value(0, 99, 0);
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x00);
        changed.write_into(order, &mut payload);
        crate::pvdata::encode::encode_pv_field_with_bitset(
            &delta,
            &intro,
            &changed,
            0,
            order,
            &mut payload,
        );
        let frame = synth_frame(Command::PutGet, order, payload);

        let res = handle_put_get(
            &frame,
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &x509_cred("MyCA"),
            &discard_exec_fin(),
        )
        .await;
        assert!(
            res.is_err(),
            "PUT_GET data against a PUT-initialised IOID must be a protocol error"
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
    #[epics_macros_rs::epics_test]
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
            non_monitor_op_state(std::sync::Arc::new(intro.clone()), OpKind::PutGet, mask),
        );
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops,
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();

        // PUT_GET data-phase frame, default putGet subcommand 0x00 (write
        // then read back). 0x40/0x80 are the read-only getGet/getPut
        // subcommands that carry no put payload, so the write+readback
        // this test exercises uses 0x00 — the subcommand op_put_get sends.
        // sid + ioid + subcmd + changed-bitset + delta(field a → 55).
        let bit_a = intro.bit_for_path("a").expect("a has a bit");
        let mut changed = BitSet::new();
        changed.set(bit_a);
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x00);
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
            &frame,
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &x509_cred("MyCA"),
            &discard_exec_fin(),
        )
        .await
        .expect("handle_put_get ok");

        let resp = rx.recv().await.expect("PUT_GET response emitted");
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

    /// The command-local last-request bit (`subcmd & 0x10`, `QOS_DESTROY`) on
    /// a PROCESS data frame is the EPICS `lastRequest()` rider — the
    /// ChannelProcess client sends `QOS_DESTROY` to mean "process this, then
    /// destroy" (`clientContextImpl.cpp:548-570`). Pre-fix the handler treated
    /// it as a pure destroy and returned before `process_checked` ran, so the
    /// client received no processDone reply. The op must execute, reply, and
    /// only then release its IOID via the deferred completion owner.
    #[epics_macros_rs::epics_test]
    async fn pva_process_last_request_executes_then_defers_destroy() {
        let order = ByteOrder::Little;
        let (sid, ioid) = (1u32, 700u32);
        let src = AuthorityGatedSource::new();
        let process_hits = std::sync::Arc::clone(&src.process_hits);
        let source: DynSource = std::sync::Arc::new(src);

        let mut channels = primed_process_channels(sid, ioid, source.clone());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let (exec_fin_tx, mut exec_fin_rx) = mpsc::unbounded_channel::<ExecFinished>();
        let config = PvaServerConfig::default();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();

        // PROCESS data frame carrying the last-request bit (QOS_DESTROY = 0x10).
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x10);
        let frame = synth_frame(Command::Process, order, payload);

        handle_process(
            &frame,
            &tx,
            &mut channels,
            order,
            &config,
            &mut TypeCache::new(),
            peer,
            &x509_cred("MyCA"),
            &exec_fin_tx,
        )
        .await
        .expect("handle_process ok");

        // The process hook ran and a status reply was emitted — the
        // last-request frame was NOT swallowed as a pure destroy.
        let resp = rx
            .recv()
            .await
            .expect("PROCESS reply emitted for a last-request frame");
        let (rframe, _) = try_parse_frame(&resp)
            .expect("frame parses")
            .expect("complete frame");
        let mut cur = rframe.cursor();
        let _ioid = cur.get_u32(order).expect("ioid");
        let _subcmd = cur.get_u8().expect("subcmd");
        let status = Status::decode(&mut cur, order).expect("status");
        assert!(
            status.is_success(),
            "a last-request PROCESS must execute and reply success"
        );
        assert_eq!(
            process_hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the process hook runs for a last-request PROCESS"
        );

        // IOID stays reserved until the reply completes, then the completion
        // owner frees it (pvxs cleanup() after the reply).
        assert!(
            channels.get(&sid).unwrap().ops.contains_key(&ioid),
            "the op is reserved until its reply is sent"
        );
        let fin = exec_fin_rx
            .recv()
            .await
            .expect("process task signals completion");
        apply_exec_finish(&mut channels, fin);
        assert!(
            !channels.get(&sid).unwrap().ops.contains_key(&ioid),
            "the last-request PROCESS op is freed after its reply"
        );
    }

    /// Same lastRequest() rider for PUT_GET: the ChannelPutGet client sends
    /// `QOS_DESTROY` (here with the `0x40` readback bit, `subcmd = 0x50`) to
    /// mean "run this, then destroy" (`clientContextImpl.cpp:1262-1288`).
    /// Pre-fix the handler treated the bit as a pure destroy, so the client
    /// got no write, no readback, and no status reply. The op must execute,
    /// reply, and only then release its IOID.
    #[epics_macros_rs::epics_test]
    async fn pva_put_get_last_request_executes_then_defers_destroy() {
        let order = ByteOrder::Little;
        let (sid, ioid) = (1u32, 701u32);
        let source: DynSource = std::sync::Arc::new(AuthorityGatedSource::new());

        let intro = three_field_intro();
        let mask = BitSet::all_set(intro.total_bits());
        let mut ops = HashMap::new();
        ops.insert(
            ioid,
            non_monitor_op_state(std::sync::Arc::new(intro.clone()), OpKind::PutGet, mask),
        );
        let mut channels = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops,
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let (exec_fin_tx, mut exec_fin_rx) = mpsc::unbounded_channel::<ExecFinished>();
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();

        // PUT_GET last-request: readback (0x40) | QOS_DESTROY (0x10) = 0x50.
        let bit_a = intro.bit_for_path("a").expect("a has a bit");
        let mut changed = BitSet::new();
        changed.set(bit_a);
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x50);
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
            &frame,
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &x509_cred("MyCA"),
            &exec_fin_tx,
        )
        .await
        .expect("handle_put_get ok");

        let resp = rx
            .recv()
            .await
            .expect("PUT_GET reply emitted for a last-request frame");
        let (rframe, _) = try_parse_frame(&resp)
            .expect("frame parses")
            .expect("complete frame");
        let mut cur = rframe.cursor();
        let _ioid = cur.get_u32(order).expect("ioid");
        let rsub = cur.get_u8().expect("subcmd");
        let status = Status::decode(&mut cur, order).expect("status");
        assert!(
            status.is_success(),
            "a last-request PUT_GET must execute and reply success"
        );
        assert_eq!(
            rsub, 0x50,
            "the reply echoes the last-request PUT_GET subcmd"
        );

        assert!(
            channels.get(&sid).unwrap().ops.contains_key(&ioid),
            "the op is reserved until its reply is sent"
        );
        let fin = exec_fin_rx
            .recv()
            .await
            .expect("put_get task signals completion");
        apply_exec_finish(&mut channels, fin);
        assert!(
            !channels.get(&sid).unwrap().ops.contains_key(&ioid),
            "the last-request PUT_GET op is freed after its reply"
        );
    }

    /// pvxs `serverintrospect.cpp:159`: GET_FIELD's guard is the
    /// composite `if(!chan || opByIOID.find(ioid)!=opByIOID.end())`.
    /// Both arms log and silently return. Our prior fix only
    /// covered the !chan branch; an IOID collision with an active
    /// GET/PUT/MONITOR/RPC in the same channel still fired back a
    /// fabricated introspection reply, polluting the wire conversation
    /// on the busy IOID.
    #[epics_macros_rs::epics_test]
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
                intro: std::sync::Arc::new(FieldDesc::Variant),
                kind: OpKind::Get,
                monitor_started: false,
                monitor_abort: None,
                mask: BitSet::new(),
                put_mask: None,
                monitor_window: None,
                monitor_window_notify: None,
                monitor_paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                monitor_resume: Arc::new(tokio::sync::Notify::new()),
                monitor_wm: None,
                monitor_wm_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
                monitor_op_id: next_op_id(),
                monitor_filters: Arc::new(
                    epics_base_rs::server::database::filters::FilterChain::new(),
                ),
                pv_request: None,
                monitor_options: crate::server_native::source::MonitorOptions::default(),
                data_task_abort: None,
                monitor_start_ctl: None,
                exec_state: ExecState::Idle,
                last_request: false,
            },
        );
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(FieldDesc::Variant)),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
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

        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);
        handle_get_field(
            &frame,
            &tx,
            &mut channels,
            order,
            peer,
            &cred,
            &discard_exec_fin(),
        )
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
    #[epics_macros_rs::epics_test]
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
                introspection: Some(std::sync::Arc::new(FieldDesc::Variant)),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        crate::proto::encode_string_into("", order, &mut payload);
        let frame = synth_frame(Command::GetField, order, payload);

        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);
        handle_get_field(
            &frame,
            &tx,
            &mut channels,
            order,
            peer,
            &cred,
            &discard_exec_fin(),
        )
        .await
        .expect("handler returns Ok");

        let resp = rx
            .try_recv()
            .expect("clean GET_FIELD must emit introspection reply");
        // ioid (4) + status (1 + ...) + type descriptor
        assert!(resp.len() > PvaHeader::SIZE + 4);
    }

    /// Per-channel report attribution wiring: a GET_FIELD request charges
    /// the FULL framed inbound length (`PvaHeader::SIZE + body`) to the
    /// channel's `statRx` and the reply to its `statTx` (pvxs
    /// serverintrospect.cpp:45/164, where `rxlen = 8u + body`). Drives the
    /// real handler and reads back the SHARED `ChannelStat` Arc the report
    /// would observe, so a future send site that bypasses `chan_tx` (or an
    /// rx charge that drops the 8-byte header) regresses this test.
    #[epics_macros_rs::epics_test]
    async fn get_field_attributes_tx_rx_to_channel_stat() {
        use crate::pvdata::FieldDesc;
        use crate::server_native::SharedSource;
        use std::sync::Arc;
        use std::sync::atomic::Ordering;

        let order = ByteOrder::Little;
        let sid: u32 = 7;
        let ioid: u32 = 4242;

        let source: DynSource = Arc::new(SharedSource::new());
        // Keep our own handle on the channel's stat — this is the exact
        // Arc a `PeerEntry::channel_opened` registration would share with
        // the report.
        let stat = crate::server_native::peers::ChannelStat::new("dut".into());

        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(FieldDesc::Variant)),
                source: source.clone(),
                stat: stat.clone(),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        crate::proto::encode_string_into("", order, &mut payload);
        let frame = synth_frame(Command::GetField, order, payload);
        let inbound_body = frame.payload.len();

        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);
        handle_get_field(
            &frame,
            &tx,
            &mut channels,
            order,
            peer,
            &cred,
            &discard_exec_fin(),
        )
        .await
        .expect("handler returns Ok");

        let resp = rx
            .try_recv()
            .expect("clean GET_FIELD must emit introspection reply");

        assert_eq!(
            stat.rx.load(Ordering::Relaxed),
            (PvaHeader::SIZE + inbound_body) as u64,
            "GET_FIELD framed length (header + body) must be charged to the channel statRx (pvxs rxlen = 8 + body)"
        );
        assert_eq!(
            stat.tx.load(Ordering::Relaxed),
            resp.len() as u64,
            "GET_FIELD reply must be charged to the channel statTx"
        );
    }

    /// Owner math for per-channel inbound op-RX accounting: `add_op_rx` must
    /// charge the FULL framed length `PvaHeader::SIZE + body`, matching
    /// pvxs `rxlen = 8u + evbuffer_get_length(segBuf)`. Every op handler
    /// (PUT_GET / PROCESS / ARRAY / GET / PUT / MONITOR / RPC / GET_FIELD)
    /// funnels its inbound frame through this one method, so a body-only
    /// charge (the pre-fix `add_rx(frame.payload.len())`) would 8-byte
    /// under-count every op reported under a live channel. Driving the
    /// single owner directly keeps the boundary anchored even as handlers
    /// are added.
    #[test]
    fn add_op_rx_charges_framed_length_header_plus_body() {
        use std::sync::atomic::Ordering;
        let stat = crate::server_native::peers::ChannelStat::new("dut".into());
        // 13-byte body → framed length must be header (8) + 13 = 21.
        let frame = synth_frame(Command::Get, ByteOrder::Little, vec![0u8; 13]);
        stat.add_op_rx(&frame);
        assert_eq!(
            stat.rx.load(Ordering::Relaxed),
            (PvaHeader::SIZE + 13) as u64,
            "per-channel op RX must be 8-byte header + body, not body alone"
        );
        // A second frame accumulates, proving `+=` semantics, not `=`.
        let frame2 = synth_frame(Command::Monitor, ByteOrder::Little, vec![0u8; 5]);
        stat.add_op_rx(&frame2);
        assert_eq!(
            stat.rx.load(Ordering::Relaxed),
            (PvaHeader::SIZE + 13 + PvaHeader::SIZE + 5) as u64,
            "successive op frames accumulate framed lengths"
        );
    }

    /// Sub-defect B boundary: the 16-byte DESTROY_CHANNEL reply is charged
    /// to the channel `statTx` for a server-initiated close (pvxs
    /// `ServerChannelControl::close()`, serverchan.cpp:151-152) but NOT for
    /// a client-initiated DESTROY_CHANNEL (pvxs
    /// `ServerConn::handle_DESTROY_CHANNEL()`, serverchan.cpp:404-411 —
    /// "don't bother to increment for channel"). Both drop the per-channel
    /// report entry. One case per `DestroyCause` boundary; before the fix a
    /// single finalizer charged the channel reply unconditionally, so the
    /// client case over-counted.
    #[epics_macros_rs::epics_test]
    async fn destroy_channel_tx_attribution_splits_by_cause() {
        use std::sync::atomic::Ordering;
        let order = ByteOrder::Little;
        for (cause, charges_channel_tx) in [
            (DestroyCause::ServerInitiated, true),
            (DestroyCause::ClientInitiated, false),
        ] {
            let source: DynSource = Arc::new(crate::server_native::SharedSource::new());
            let sid = 1u32;
            let cid = 10u32;
            let stat = crate::server_native::peers::ChannelStat::new("dut".into());
            let mut channels: HashMap<u32, ChannelState> = HashMap::new();
            channels.insert(
                sid,
                ChannelState {
                    name: "dut".into(),
                    cid,
                    sid,
                    introspection: None,
                    source: source.clone(),
                    stat: stat.clone(),
                    open_cred: ClientCredentials::anonymous(TEST_PEER),
                    ops: HashMap::new(),
                },
            );
            let peer_entry = crate::server_native::peers::PeerEntry::new(false);
            peer_entry.channel_opened(sid, stat.clone());

            let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
            let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
            let teardown = ChannelTeardownCtx {
                tx: &tx,
                order,
                peer,
                peer_entry: &peer_entry,
            };

            let torn = finalize_channel_destroy(sid, cid, cause, &mut channels, &teardown).await;
            assert!(torn, "{cause:?}: the live channel was torn down");

            let frame = rx
                .try_recv()
                .unwrap_or_else(|_| panic!("{cause:?}: DESTROY_CHANNEL frame must be emitted"));
            assert_eq!(frame.len(), PvaHeader::SIZE + 8, "{cause:?}: 16-byte reply");

            let expected_channel_tx = if charges_channel_tx {
                frame.len() as u64
            } else {
                0
            };
            assert_eq!(
                stat.tx.load(Ordering::Relaxed),
                expected_channel_tx,
                "{cause:?}: per-channel statTx (server charges the reply, client does not — pvxs serverchan.cpp:152 vs :410)"
            );
            assert_eq!(
                peer_entry.channels.load(Ordering::SeqCst),
                0,
                "{cause:?}: per-channel report entry + live count dropped"
            );
            assert!(
                channels.is_empty(),
                "{cause:?}: connection channel-table entry removed"
            );
        }
    }

    /// GET_FIELD slow path on a channel whose source returns no
    /// descriptor must reply `Status::Error` with NO descriptor, matching
    /// pvxs `ServerIntrospectControl::error` → `doReply(nullptr, Status::
    /// Error)` (`serverintrospect.cpp:83-87`) and the `if(type)` guard at
    /// `:41-42`. The old `unwrap_or(FieldDesc::Variant)` reported success
    /// and fabricated a Variant type tree.
    #[epics_macros_rs::epics_test]
    async fn get_field_none_introspection_replies_error_no_descriptor() {
        use crate::decode::{decode_get_field_response, try_parse_frame};
        use crate::server_native::SharedSource;
        use std::sync::Arc;

        let order = ByteOrder::Little;
        let sid: u32 = 7;
        let ioid: u32 = 5555;

        // SharedSource with no PV named "dut": get_introspection → None.
        let source: DynSource = Arc::new(SharedSource::new());

        // Channel exists but introspection was never cached → slow path.
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: None,
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        crate::proto::encode_string_into("", order, &mut payload);
        let frame = synth_frame(Command::GetField, order, payload);

        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);
        // The reserved GET_FIELD op (holding the task's abort guard) lives
        // in `channels`, which stays in scope until after the reply below,
        // so the slow-path task is not aborted before it replies.
        handle_get_field(
            &frame,
            &tx,
            &mut channels,
            order,
            peer,
            &cred,
            &discard_exec_fin(),
        )
        .await
        .expect("handler returns Ok");

        // Slow path spawns the source call; await its reply.
        let resp = rx.recv().await.expect("GET_FIELD slow-path reply emitted");
        let (rframe, _) = try_parse_frame(&resp).unwrap().unwrap();
        let decoded = decode_get_field_response(&rframe).unwrap();
        assert_eq!(decoded.ioid, ioid);
        assert!(
            !decoded.status.is_success(),
            "introspection failure must reply Status::Error, not OK"
        );
        assert!(
            decoded.introspection.is_none(),
            "no descriptor on failure (pvxs if(type) guard); must not fabricate Variant"
        );
    }

    /// Companion: a successful GET_FIELD slow path still returns the
    /// exact descriptor the source provided (no regression from the
    /// error-path fix).
    #[epics_macros_rs::epics_test]
    async fn get_field_slow_path_returns_source_descriptor() {
        use crate::decode::{decode_get_field_response, try_parse_frame};
        use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
        use crate::server_native::SharedSource;
        use crate::server_native::shared_pv::SharedPV;
        use std::sync::Arc;

        let order = ByteOrder::Little;
        let sid: u32 = 7;
        let ioid: u32 = 5556;

        let intro = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let pv = SharedPV::new();
        let mut initial = PvStructure::new("epics:nt/NTScalar:1.0");
        initial
            .fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        pv.open(intro.clone(), PvField::Structure(initial)).unwrap();
        let shared = SharedSource::new();
        shared.add("dut", pv);
        let source: DynSource = Arc::new(shared);

        // Channel introspection not cached → exercise the slow path.
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: None,
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        crate::proto::encode_string_into("", order, &mut payload);
        let frame = synth_frame(Command::GetField, order, payload);

        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);
        // The reserved GET_FIELD op (holding the task's abort guard) lives
        // in `channels`, which stays in scope until after the reply below,
        // so the slow-path task is not aborted before it replies.
        handle_get_field(
            &frame,
            &tx,
            &mut channels,
            order,
            peer,
            &cred,
            &discard_exec_fin(),
        )
        .await
        .expect("handler returns Ok");

        let resp = rx.recv().await.expect("GET_FIELD slow-path reply emitted");
        let (rframe, _) = try_parse_frame(&resp).unwrap().unwrap();
        let decoded = decode_get_field_response(&rframe).unwrap();
        assert!(decoded.status.is_success(), "valid descriptor → Status::Ok");
        assert_eq!(
            decoded.introspection.as_ref(),
            Some(&intro),
            "slow path must return the exact source descriptor"
        );
    }

    /// A slow GET_FIELD reserves its IOID before spawning the source
    /// introspection, so a second GET_FIELD reusing the same `(sid, ioid)`
    /// while the first is still in flight is dropped silently and never
    /// spawns a second task — only one reply reaches the client. Mirrors
    /// pvxs `opByIOID.find(ioid)!=end()` rejection (serverintrospect.cpp:157)
    /// once the introspect op is inserted Executing (:164-178).
    #[epics_macros_rs::epics_test]
    async fn slow_get_field_reserves_ioid_against_duplicate() {
        use crate::pvdata::FieldDesc;
        use crate::server_native::source::ChannelSource;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Source whose introspection blocks on a semaphore so the first
        // GET_FIELD stays in flight while the duplicate arrives, and which
        // counts how many times introspection is actually invoked.
        struct GatedIntrospectSource {
            gate: Arc<tokio::sync::Semaphore>,
            calls: Arc<AtomicUsize>,
        }
        impl ChannelSource for GatedIntrospectSource {
            async fn list_pvs(&self) -> Vec<String> {
                vec!["dut".into()]
            }
            async fn has_pv(&self, name: &str) -> bool {
                name == "dut"
            }
            async fn get_introspection(&self, _name: &str) -> Option<FieldDesc> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let _permit = self.gate.acquire().await.unwrap();
                Some(FieldDesc::Variant)
            }
            async fn get_value(&self, _name: &str) -> Option<PvField> {
                None
            }
            async fn put_value(&self, _name: &str, _v: PvField) -> Result<(), OpError> {
                Ok(())
            }
            async fn is_writable(&self, _name: &str) -> bool {
                false
            }
            async fn subscribe(&self, _name: &str) -> Option<MonitorStream<PvField>> {
                None
            }
        }

        let order = ByteOrder::Little;
        let sid: u32 = 7;
        let ioid: u32 = 9001;
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let source: DynSource = Arc::new(GatedIntrospectSource {
            gate: gate.clone(),
            calls: calls.clone(),
        });

        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: None,
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let build = || {
            let mut payload = Vec::new();
            payload.put_u32(sid, order);
            payload.put_u32(ioid, order);
            crate::proto::encode_string_into("", order, &mut payload);
            synth_frame(Command::GetField, order, payload)
        };
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // First GET_FIELD: reserves the IOID and spawns the (blocked) task.
        let frame1 = build();
        handle_get_field(
            &frame1,
            &tx,
            &mut channels,
            order,
            peer,
            &cred,
            &discard_exec_fin(),
        )
        .await
        .expect("first GET_FIELD Ok");
        assert!(
            channels[&sid].ops.contains_key(&ioid),
            "slow GET_FIELD must reserve its IOID in ch.ops"
        );

        // Duplicate GET_FIELD on the same IOID while the first is in flight:
        // must be dropped without spawning a second introspection.
        let frame2 = build();
        handle_get_field(
            &frame2,
            &tx,
            &mut channels,
            order,
            peer,
            &cred,
            &discard_exec_fin(),
        )
        .await
        .expect("duplicate GET_FIELD Ok (silently dropped)");

        // Release the first task and let it reply.
        gate.add_permits(1);
        let _first = rx.recv().await.expect("the one reserved GET_FIELD replies");
        // No second reply: drain briefly and assert empty.
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "a duplicate-IOID GET_FIELD must not produce a second reply"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the duplicate GET_FIELD must not invoke source introspection a second time"
        );
    }

    // ---- raw monitor re-encode must not fabricate a missing
    // overrun bitset ------------------------------------------------
    //
    // `RawMonitorEvent.body_bytes` is the `changed | value | overrun`
    // triplet. The trailing overrun bitset is a REQUIRED part of the
    // MONITOR DATA wire format: pvxs reads it unconditionally
    // (`clientmon.cpp:550` `from_wire(M, overrun)`) and disconnects
    // when the message is not good afterwards (`clientmon.cpp:596`).
    // Before the fix, `reencode_raw_monitor` mapped a failed overrun
    // decode to `BitSet::new()`, fabricating a valid empty overrun from
    // a truncated/corrupt upstream body — so the SAME event meant
    // different things depending only on the negotiated byte order:
    // the same-endian forward carries the malformed bytes through
    // verbatim (downstream detects it), while the cross-endian
    // re-encode silently "repaired" it.

    /// Build a raw MONITOR DATA body for the three-`Int` test structure.
    /// `with_overrun` controls whether the required trailing overrun
    /// bitset is present; omitting it models a truncated upstream body.
    #[cfg(test)]
    fn bfr9_raw_body(order: ByteOrder, with_overrun: bool) -> bytes::Bytes {
        let intro = three_field_intro();
        let changed = BitSet::all_set(intro.total_bits());
        let value = three_field_value(1, 2, 3);
        let mut body = Vec::new();
        changed.write_into(order, &mut body);
        crate::pvdata::encode::encode_pv_field_with_bitset(
            &value, &intro, &changed, 0, order, &mut body,
        );
        if with_overrun {
            // Mark bit 1 (field `a`) so a NON-EMPTY overrun is exercised.
            let mut overrun = BitSet::new();
            overrun.set(1);
            overrun.write_into(order, &mut body);
        }
        bytes::Bytes::from(body)
    }

    /// Cross-endian re-encode of a body whose required overrun trailer
    /// is missing must return an error — NOT fabricate an empty overrun
    /// and emit a "valid" frame from corruption.
    #[test]
    fn bfr9_reencode_missing_overrun_returns_error() {
        let intro = three_field_intro();
        let ev = crate::server_native::RawMonitorEvent {
            body_bytes: bfr9_raw_body(ByteOrder::Little, false),
            byte_order: ByteOrder::Little,
            type_changed: false,
        };
        // Downstream negotiated the opposite order → re-encode path.
        let result = reencode_raw_monitor(7, &intro, &ev, ByteOrder::Big);
        let err =
            result.expect_err("missing overrun trailer must error, not fabricate an empty bitset");
        assert!(
            err.contains("overrun"),
            "error must name the overrun bitset, got: {err}"
        );
    }

    /// A NON-EMPTY overrun bitset must survive a cross-endian re-encode:
    /// the bit set under the upstream order is still set after decoding
    /// the re-encoded frame under the downstream order.
    #[test]
    fn bfr9_reencode_nonempty_overrun_survives_cross_endian() {
        let intro = three_field_intro();
        let ev = crate::server_native::RawMonitorEvent {
            body_bytes: bfr9_raw_body(ByteOrder::Little, true),
            byte_order: ByteOrder::Little,
            type_changed: false,
        };
        let down = ByteOrder::Big;
        let frame_bytes =
            reencode_raw_monitor(7, &intro, &ev, down).expect("valid triplet re-encodes");
        let (frame, _) = try_parse_frame(&frame_bytes)
            .expect("re-encoded frame parses")
            .expect("complete frame");
        assert_eq!(
            frame.header.command,
            Command::Monitor.code(),
            "re-encoded frame is a MONITOR"
        );
        let mut cur = frame.cursor();
        assert_eq!(cur.get_u32(down).expect("ioid"), 7, "ioid preserved");
        assert_eq!(cur.get_u8().expect("subcmd"), 0x00, "DATA subcmd");
        let changed = BitSet::decode(&mut cur, down).expect("changed bitset");
        let value =
            crate::pvdata::encode::decode_pv_field_with_bitset(&intro, &changed, 0, &mut cur, down)
                .expect("value");
        assert_eq!(
            three_field_extract(&value),
            (1, 2, 3),
            "value survives the cross-endian round-trip"
        );
        let overrun = BitSet::decode(&mut cur, down).expect("overrun bitset present");
        assert!(
            overrun.get(1),
            "the non-empty overrun bit must survive cross-endian re-encode"
        );
    }

    /// Agreement check: the same-endian forward does NOT fabricate. A
    /// truncated body (missing overrun) is forwarded byte-for-byte, so
    /// a downstream client sees the same malformed bytes the
    /// cross-endian path now refuses to "repair". Both paths therefore
    /// decline to invent a valid empty overrun.
    #[test]
    fn bfr9_same_endian_forward_does_not_fabricate() {
        let order = ByteOrder::Little;
        let body = bfr9_raw_body(order, false);
        let ev = crate::server_native::RawMonitorEvent {
            body_bytes: body.clone(),
            byte_order: order,
            type_changed: false,
        };
        let frame_bytes = build_monitor_payload_raw(7, &ev, order);
        // Layout: [8-byte PVA header][4-byte ioid][1-byte subcmd][body].
        let prefix = 8 + 4 + 1;
        assert_eq!(
            &frame_bytes[prefix..],
            &body[..],
            "same-endian path forwards the truncated body verbatim — no fabricated overrun trailer"
        );
    }

    // ---- a cross-endian raw monitor re-encode failure is a
    // terminal protocol boundary, not a silently dropped event -------
    //
    // `raw_monitor_frame` owns the single malformed-raw policy: a body
    // that cannot be re-encoded under the downstream order yields
    // `Terminate` (a MONITOR error frame ends the stream) rather than a
    // debug-logged drop. pvxs resets the connection when a monitor
    // message is not good (`clientmon.cpp:596`); the same-endian forward
    // carries malformed bytes through verbatim so the downstream peer
    // fails at its own boundary. Both decline to invent a valid frame.

    /// Assert `frame_bytes` is a terminal MONITOR error: command
    /// MONITOR, subcmd `0x10` (finish), non-success status.
    #[cfg(test)]
    fn bfr14_assert_monitor_error(frame_bytes: &[u8], order: ByteOrder) {
        let (frame, _) = try_parse_frame(frame_bytes)
            .expect("terminate frame parses")
            .expect("complete frame");
        assert_eq!(
            frame.header.command,
            Command::Monitor.code(),
            "terminate frame is a MONITOR"
        );
        let mut cur = frame.cursor();
        let _ioid = cur.get_u32(order).expect("ioid");
        assert_eq!(cur.get_u8().expect("subcmd"), 0x10, "FINISH subcmd");
        let status = Status::decode(&mut cur, order).expect("status");
        assert!(
            !status.is_success(),
            "terminate frame must carry a non-success status"
        );
    }

    /// Cross-endian raw monitor whose changed bitset is truncated must
    /// terminate the monitor with an error, not be silently dropped.
    #[test]
    fn bfr14_cross_endian_truncated_changed_bitset_terminates() {
        let intro = three_field_intro();
        // Size prefix claims 5 bytes follow, but none do → the changed
        // bitset (the first decode) fails.
        let ev = crate::server_native::RawMonitorEvent {
            body_bytes: bytes::Bytes::from_static(&[0x05]),
            byte_order: ByteOrder::Little,
            type_changed: false,
        };
        match raw_monitor_frame(7, &intro, &ev, ByteOrder::Big) {
            RawMonitorFrame::Terminate { frame, reason } => {
                assert!(
                    reason.contains("changed bitset"),
                    "reason names the truncation point, got: {reason}"
                );
                bfr14_assert_monitor_error(&frame, ByteOrder::Big);
            }
            RawMonitorFrame::Forward(_) => {
                panic!("a truncated changed bitset must terminate the monitor, not forward")
            }
        }
    }

    /// Cross-endian raw monitor whose partial value is truncated must
    /// terminate the monitor with an error.
    #[test]
    fn bfr14_cross_endian_truncated_value_terminates() {
        let intro = three_field_intro();
        let changed = BitSet::all_set(intro.total_bits());
        let mut body = Vec::new();
        changed.write_into(ByteOrder::Little, &mut body);
        // Only 2 value bytes where 3 marked `Int` fields need 12 → the
        // value decode fails.
        body.extend_from_slice(&[0u8, 0u8]);
        let ev = crate::server_native::RawMonitorEvent {
            body_bytes: bytes::Bytes::from(body),
            byte_order: ByteOrder::Little,
            type_changed: false,
        };
        match raw_monitor_frame(7, &intro, &ev, ByteOrder::Big) {
            RawMonitorFrame::Terminate { frame, reason } => {
                assert!(
                    reason.contains("value"),
                    "reason names the truncation point, got: {reason}"
                );
                bfr14_assert_monitor_error(&frame, ByteOrder::Big);
            }
            RawMonitorFrame::Forward(_) => {
                panic!("a truncated partial value must terminate the monitor, not forward")
            }
        }
    }

    /// One documented policy for malformed raw, not byte-order-dependent
    /// behaviour: the SAME truncated body (missing overrun trailer)
    /// forwards verbatim same-endian but terminates cross-endian. Both
    /// decline to fabricate a valid frame.
    #[test]
    fn bfr14_same_and_cross_endian_malformed_one_policy() {
        let intro = three_field_intro();
        let body = bfr9_raw_body(ByteOrder::Little, false); // missing overrun
        let ev = crate::server_native::RawMonitorEvent {
            body_bytes: body.clone(),
            byte_order: ByteOrder::Little,
            type_changed: false,
        };
        // Same-endian → forward the malformed body verbatim.
        match raw_monitor_frame(7, &intro, &ev, ByteOrder::Little) {
            RawMonitorFrame::Forward(frame) => {
                let prefix = 8 + 4 + 1;
                assert_eq!(
                    &frame[prefix..],
                    &body[..],
                    "same-endian forwards the malformed body verbatim"
                );
            }
            RawMonitorFrame::Terminate { .. } => panic!("same-endian must forward, not terminate"),
        }
        // Cross-endian → terminate with an error (the missing overrun
        // trailer cannot be re-encoded).
        match raw_monitor_frame(7, &intro, &ev, ByteOrder::Big) {
            RawMonitorFrame::Terminate { frame, .. } => {
                bfr14_assert_monitor_error(&frame, ByteOrder::Big);
            }
            RawMonitorFrame::Forward(_) => {
                panic!("cross-endian malformed must terminate, not forward")
            }
        }
    }

    // ─── MONITOR FINISH op cleanup ───────────────────────────
    //
    // Invariant (MUST): an op present in `ch.ops` with
    // `monitor_started == true` ⟺ its subscriber task is alive. When the
    // task ends for ANY reason (source close, descriptor change, ACL
    // deny, filter mismatch, raw re-encode terminal, panic, abort) the
    // read-loop OWNER removes the op — dropping `monitor_start_ctl`
    // (terminal `notify_monitor_start(false)`) and `monitor_abort`. The
    // task reports its identity via `MonitorFinishGuard`; the owner
    // applies it via `apply_monitor_finish`, gated on the op-instance id
    // so a stale signal cannot evict a re-INIT'd op that reused the ioid.

    /// Minimal source: serves one PV, records every
    /// `notify_monitor_start` edge, and hands MONITOR a subscription that
    /// closes immediately so the subscriber task ends as on source close.
    struct Bfr12Source {
        intro: FieldDesc,
        value: PvField,
        starts: Arc<parking_lot::Mutex<Vec<bool>>>,
    }
    impl crate::server_native::source::ChannelSource for Bfr12Source {
        async fn list_pvs(&self) -> Vec<String> {
            vec!["dut".into()]
        }
        async fn has_pv(&self, n: &str) -> bool {
            n == "dut"
        }
        async fn get_introspection(&self, _n: &str) -> Option<FieldDesc> {
            Some(self.intro.clone())
        }
        async fn get_value(&self, _n: &str) -> Option<PvField> {
            Some(self.value.clone())
        }
        async fn put_value(&self, _n: &str, _v: PvField) -> Result<(), OpError> {
            Ok(())
        }
        async fn is_writable(&self, _n: &str) -> bool {
            false
        }
        async fn subscribe(&self, _n: &str) -> Option<MonitorStream<PvField>> {
            // Sender dropped immediately → receiver closed → the subscriber
            // loop sees end-of-stream and the task ends (source close).
            let (_tx, rx) = mpsc::channel(1);
            Some(rx.into())
        }
        fn notify_monitor_start(
            &self,
            _name: &str,
            _ctx: &crate::server_native::source::ChannelContext,
            start: bool,
        ) {
            self.starts.lock().push(start);
        }
    }

    fn bfr12_anon_ctx() -> crate::server_native::source::ChannelContext {
        crate::server_native::source::ChannelContext {
            peer: "127.0.0.1:5075".parse().unwrap(),
            account: String::new(),
            method: "anonymous".into(),
            host: String::new(),
            authority: String::new(),
            roles: Vec::new(),
            pv_request: None,
            log: Default::default(),
        }
    }

    /// Build a `channels` map with one started MONITOR op (op id `op_id`,
    /// `monitor_started = true`) whose `monitor_start_ctl` has already
    /// fired the Idle→Executing edge — so the terminal Executing→Idle edge
    /// is observable when the op is removed. The op holds the ONLY
    /// `MonitorStartControl` Arc ref, so removal drops it and fires
    /// `notify_monitor_start(false)`.
    fn bfr12_started_monitor_channels(
        sid: u32,
        ioid: u32,
        op_id: u64,
        src: &DynSource,
        intro: &FieldDesc,
    ) -> HashMap<u32, ChannelState> {
        let mut op = non_monitor_op_state(
            std::sync::Arc::new(intro.clone()),
            OpKind::Monitor,
            BitSet::all_set(intro.total_bits()),
        );
        op.monitor_op_id = op_id;
        op.monitor_started = true;
        let (exec_tx, _exec_rx) = tokio::sync::watch::channel(false);
        let ctl = Arc::new(MonitorStartControl::new(
            src.clone(),
            "dut".into(),
            bfr12_anon_ctx(),
            exec_tx,
        ));
        ctl.set(true); // record the Idle→Executing edge
        op.monitor_start_ctl = Some(ctl); // op now holds the only Arc ref
        let mut ops = HashMap::new();
        ops.insert(ioid, op);
        let mut channels = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: src.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops,
            },
        );
        channels
    }

    /// Per-op MONITOR gate wiring: `MonitorStartControl::set` publishes each
    /// Executing<->Idle edge to the per-op gate driver, which calls
    /// `MonitorGate::set_active` accordingly. Drives START -> STOP -> START
    /// and asserts the gate observed the initial Idle state plus each edge
    /// in order — waiting between edges so the watch does not coalesce
    /// them (coalescing is correct for the production net-state, but a
    /// per-edge assertion needs them separated). The source-level
    /// `notify_monitor_start` edges stay in lockstep.
    #[epics_macros_rs::epics_test]
    async fn bridge118_gate_driver_follows_executing_edges() {
        use crate::server_native::source::MonitorGate;

        let intro = three_field_intro();
        let starts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let src: DynSource = Arc::new(Bfr12Source {
            intro: intro.clone(),
            value: three_field_value(0, 0, 0),
            starts: starts.clone(),
        });

        // Recording gate: every set_active(active) call lands here.
        let gate_log = Arc::new(parking_lot::Mutex::new(Vec::<bool>::new()));
        let gate_log_w = gate_log.clone();
        let gate = MonitorGate::new(move |active| {
            let log = gate_log_w.clone();
            async move {
                log.lock().push(active);
            }
        });

        let (exec_tx, mut exec_rx) = tokio::sync::watch::channel(false);
        // Stand-in for the subscriber loop, reduced to the two things that
        // matter here: it reads the executing state and applies the gate once
        // per iteration, and it wakes on every watch edge. The production
        // loop does exactly this (`let executing = *exec_rx.borrow_and_update()`
        // then `gate_driver.apply(executing).await`, with an
        // `exec_rx.changed()` select arm); `bfr12_gate_reaches_a_parked_monitor`
        // covers the same path end to end through a real MONITOR.
        let mut gate_driver = MonitorGateDriver::new(Some(gate));
        epics_base_rs::runtime::task::spawn(async move {
            loop {
                let executing = *exec_rx.borrow_and_update();
                gate_driver.apply(executing).await;
                if exec_rx.changed().await.is_err() {
                    return;
                }
            }
        });
        let ctl = MonitorStartControl::new(src.clone(), "dut".into(), bfr12_anon_ctx(), exec_tx);

        async fn wait_len(log: &Arc<parking_lot::Mutex<Vec<bool>>>, n: usize) {
            for _ in 0..200 {
                if log.lock().len() >= n {
                    return;
                }
                epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
            }
            panic!("gate log never reached {n} entries: {:?}", log.lock());
        }

        // Initial: the driver applies the watch's starting state (Idle =
        // false). Wait for it BEFORE the first edge so it is not coalesced.
        wait_len(&gate_log, 1).await;
        ctl.set(true); // START  (Idle -> Executing)
        wait_len(&gate_log, 2).await;
        ctl.set(false); // STOP / PAUSE  (Executing -> Idle)
        wait_len(&gate_log, 3).await;
        ctl.set(true); // RESUME  (Idle -> Executing)
        wait_len(&gate_log, 4).await;

        assert_eq!(
            *gate_log.lock(),
            vec![false, true, false, true],
            "gate must observe initial Idle then each Executing<->Idle edge in order"
        );
        assert_eq!(
            *starts.lock(),
            vec![true, false, true],
            "notify_monitor_start fires the same edges (only real edges, no initial apply)"
        );
    }

    /// A source that hands the subscriber a gate plus an updates stream
    /// that never produces — so the subscriber loop parks in `rx.recv()`.
    #[derive(Clone)]
    struct GatedQuietSource {
        intro: FieldDesc,
        gate_log: Arc<parking_lot::Mutex<Vec<bool>>>,
        /// Keeps the updates channel open; dropping it would end the loop.
        _keepalive: Arc<mpsc::Sender<crate::server_native::MonitorUpdate>>,
        keep: Arc<parking_lot::Mutex<Option<mpsc::Receiver<crate::server_native::MonitorUpdate>>>>,
    }
    impl crate::server_native::source::ChannelSource for GatedQuietSource {
        async fn list_pvs(&self) -> Vec<String> {
            vec!["dut".into()]
        }
        async fn has_pv(&self, n: &str) -> bool {
            n == "dut"
        }
        async fn get_introspection(&self, _n: &str) -> Option<FieldDesc> {
            Some(self.intro.clone())
        }
        async fn get_value(&self, _n: &str) -> Option<PvField> {
            None
        }
        async fn put_value(&self, _n: &str, _v: PvField) -> Result<(), OpError> {
            Ok(())
        }
        async fn is_writable(&self, _n: &str) -> bool {
            false
        }
        async fn subscribe(&self, _n: &str) -> Option<MonitorStream<PvField>> {
            None
        }
        async fn subscribe_seeded(
            &self,
            _checked: crate::server_native::source::AccessChecked,
            _ctx: crate::server_native::source::ChannelContext,
            _opts: crate::server_native::MonitorOptions,
        ) -> Option<
            crate::server_native::source::SubscriptionSeed<crate::server_native::MonitorUpdate>,
        > {
            let log = self.gate_log.clone();
            Some(crate::server_native::source::SubscriptionSeed {
                initial: None,
                updates: self.keep.lock().take()?.into(),
                on_start: Some(crate::server_native::source::MonitorGate::new(
                    move |active| {
                        let log = log.clone();
                        async move {
                            log.lock().push(active);
                        }
                    },
                )),
            })
        }
    }

    /// The gate driver is no longer a task of its own, so a START/STOP edge
    /// now reaches the source only if the subscriber loop observes it. The
    /// case that would expose a missed observation is precisely the one
    /// where nothing else can wake the loop: a monitor with no updates
    /// flowing, parked in `rx.recv()`. Drive INIT -> START -> STOP against
    /// such a monitor and require the gate to see the initial Idle plus both
    /// edges.
    ///
    /// Revert-verify: drop the per-iteration `gate_driver.apply(executing)`
    /// (keeping only the pre-loop application) and the log stalls at
    /// `[false]`.
    #[epics_macros_rs::epics_test]
    async fn bfr12_gate_reaches_a_parked_monitor() {
        let intro = three_field_intro();
        let gate_log = Arc::new(parking_lot::Mutex::new(Vec::<bool>::new()));
        let (keep_tx, keep_rx) = mpsc::channel::<crate::server_native::MonitorUpdate>(4);
        let src: DynSource = Arc::new(GatedQuietSource {
            intro: intro.clone(),
            gate_log: gate_log.clone(),
            _keepalive: Arc::new(keep_tx),
            keep: Arc::new(parking_lot::Mutex::new(Some(keep_rx))),
        });

        let (wire_tx, _wire_rx) = mpsc::channel::<Vec<u8>>(64);
        let (mon_fin_tx, _mon_fin_rx) = mpsc::unbounded_channel::<MonitorFinished>();
        let (_join, start_ctl) = spawn_monitor_subscriber(MonitorSubscriberArgs {
            sid: 1,
            ioid: 2,
            pv_name: "dut".into(),
            intro: std::sync::Arc::new(intro.clone()),
            mask: BitSet::with_capacity(intro.total_bits()),
            tx: ChannelTx::new(
                wire_tx,
                crate::server_native::peers::ChannelStat::new("dut".into()),
            ),
            src: src.clone(),
            queue_depth: 4,
            high_watermark: 0,
            mon_ctx: bfr12_anon_ctx(),
            window: None,
            window_notify: None,
            filters: Arc::new(Default::default()),
            monitor_options: Default::default(),
            wm_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            monitor_op_id: 7,
            wm_levels: None,
            mon_fin_tx,
            out_order: fixed_out_order(ByteOrder::Little),
        });

        async fn wait_len(log: &Arc<parking_lot::Mutex<Vec<bool>>>, n: usize) {
            for _ in 0..400 {
                if log.lock().len() >= n {
                    return;
                }
                epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
            }
            panic!("gate log never reached {n} entries: {:?}", log.lock());
        }

        // Initial Idle, applied where the driver used to be spawned. Wait for
        // it before the first edge so the watch cannot coalesce the two.
        wait_len(&gate_log, 1).await;
        start_ctl.set(true); // START: resume the upstream
        wait_len(&gate_log, 2).await;
        start_ctl.set(false); // STOP: suspend it again, loop still parked
        wait_len(&gate_log, 3).await;

        assert_eq!(
            *gate_log.lock(),
            vec![false, true, false],
            "a parked monitor must still relay initial Idle and both edges to \
             its source gate"
        );
    }

    /// The guard's `Drop` reports completion to the owner — the mechanism
    /// that makes the op-removal invariant hold on EVERY task exit.
    /// Revert-verify: delete the `Drop` body and `try_recv` returns Empty.
    #[test]
    fn bfr12_finish_guard_signals_on_drop() {
        let (tx, mut rx) = mpsc::unbounded_channel::<MonitorFinished>();
        {
            let _g = MonitorFinishGuard {
                tx: tx.clone(),
                fin: MonitorFinished {
                    sid: 3,
                    ioid: 11,
                    op_id: 77,
                },
            };
        } // guard drops here
        let got = rx.try_recv().expect("guard Drop must signal the owner");
        assert_eq!((got.sid, got.ioid, got.op_id), (3, 11, 77));
    }

    /// The owner removes only the op INSTANCE that signaled: a stale
    /// signal (different `op_id`, e.g. from a late-dropped aborted task
    /// whose ioid was re-INIT'd) must not evict the live op, and a
    /// matching signal removes it and fires the terminal start edge once.
    /// Revert-verify: make `apply_monitor_finish` remove unconditionally
    /// (drop the op_id guard) and the first assertion fails.
    #[test]
    fn bfr12_apply_finish_ignores_stale_op_id() {
        let intro = three_field_intro();
        let starts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let src: DynSource = Arc::new(Bfr12Source {
            intro: intro.clone(),
            value: three_field_value(0, 0, 0),
            starts: starts.clone(),
        });
        let (sid, ioid, op_id) = (1u32, 7u32, 12_345u64);
        let mut channels = bfr12_started_monitor_channels(sid, ioid, op_id, &src, &intro);

        // Stale signal: same (sid, ioid) but a DIFFERENT instance id.
        apply_monitor_finish(
            &mut channels,
            MonitorFinished {
                sid,
                ioid,
                op_id: op_id + 999,
            },
        );
        assert!(
            channels[&sid].ops.contains_key(&ioid),
            "a stale op_id must NOT evict the live op (ABA guard)"
        );
        assert_eq!(
            *starts.lock(),
            vec![true],
            "a rejected stale signal fires no terminal notify_monitor_start"
        );

        // Matching signal removes the op and fires the terminal edge once.
        apply_monitor_finish(&mut channels, MonitorFinished { sid, ioid, op_id });
        assert!(
            !channels[&sid].ops.contains_key(&ioid),
            "a matching op_id removes the op from ch.ops"
        );
        assert_eq!(
            *starts.lock(),
            vec![true, false],
            "removal drops monitor_start_ctl → terminal notify_monitor_start(false) once"
        );
    }

    /// After a server-originated FINISH the ioid is freed in `ch.ops`, so
    /// the duplicate-INIT fatal gate (`ch.ops.contains_key(&ioid)` in
    /// `handle_op`) no longer trips and a re-INIT of the same ioid is
    /// accepted as a fresh operation. Revert-verify: skip the removal and
    /// `contains_key` stays true (pre-fix: re-INIT rejected as duplicate).
    #[test]
    fn bfr12_reinit_after_finish_accepted_as_fresh() {
        let intro = three_field_intro();
        let starts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let src: DynSource = Arc::new(Bfr12Source {
            intro: intro.clone(),
            value: three_field_value(0, 0, 0),
            starts: starts.clone(),
        });
        let (sid, ioid, op_id) = (1u32, 7u32, 555u64);
        let mut channels = bfr12_started_monitor_channels(sid, ioid, op_id, &src, &intro);

        assert!(
            channels[&sid].ops.contains_key(&ioid),
            "precondition: a started monitor op trips the duplicate-INIT gate"
        );
        apply_monitor_finish(&mut channels, MonitorFinished { sid, ioid, op_id });
        assert!(
            !channels[&sid].ops.contains_key(&ioid),
            "after FINISH the ioid is free → a re-INIT is accepted as fresh, not duplicate"
        );
    }

    /// Load-bearing regression: START a MONITOR through `handle_op`, let
    /// the source-close end the subscriber task, and prove the task
    /// signals its read-loop owner so the op is removed and the terminal
    /// start-control edge fires exactly once. This exercises the guard
    /// install at the spawn site end-to-end.
    ///
    /// Revert-verify: drop the `MonitorFinishGuard` install in the spawned
    /// task and `mon_fin_rx.recv()` below times out — the op leaks in
    /// `ch.ops` with `monitor_started == true` and `notify_monitor_start`
    /// never reaches `false`, exactly the pre-fix behaviour.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bfr12_monitor_source_close_signals_owner_and_removes_op() {
        use crate::server_native::runtime::PvaServerConfig;

        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 900;
        let intro = three_field_intro();
        let starts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let source: DynSource = Arc::new(Bfr12Source {
            intro: intro.clone(),
            value: three_field_value(0, 0, 0),
            starts: starts.clone(),
        });

        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let (mon_fin_tx, mut mon_fin_rx) = mpsc::unbounded_channel::<MonitorFinished>();
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // MONITOR INIT (subcmd 0x08): empty pvRequest → full monitor.
        let req_val = PvField::Structure(PvStructure {
            struct_id: String::new(),
            fields: vec![],
        });
        let req_desc = req_val.descriptor();
        let mut init_payload = Vec::new();
        init_payload.put_u32(sid, order);
        init_payload.put_u32(ioid, order);
        init_payload.put_u8(0x08);
        crate::pvdata::encode::encode_type_desc(&req_desc, order, &mut init_payload);
        crate::pvdata::encode::encode_pv_field(&req_val, &req_desc, order, &mut init_payload);
        let init_frame = synth_frame(Command::Monitor, order, init_payload);
        handle_op(
            &init_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &mon_fin_tx,
            &discard_exec_fin(),
        )
        .await
        .expect("MONITOR INIT ok");
        let _ = rx.recv().await.expect("INIT reply");

        // The op-instance id minted at INIT — the guard signals THIS id.
        let op_id = channels[&sid].ops[&ioid].monitor_op_id;

        // MONITOR START (subcmd 0x44 = start | process) spawns the task
        // and fires the Idle→Executing edge.
        let mut start_payload = Vec::new();
        start_payload.put_u32(sid, order);
        start_payload.put_u32(ioid, order);
        start_payload.put_u8(0x44);
        let start_frame = synth_frame(Command::Monitor, order, start_payload);
        handle_op(
            &start_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &mon_fin_tx,
            &discard_exec_fin(),
        )
        .await
        .expect("MONITOR START ok");

        assert!(
            channels[&sid].ops.contains_key(&ioid),
            "op is live in ch.ops immediately after START"
        );
        assert_eq!(
            *starts.lock(),
            vec![true],
            "START fires the Idle→Executing edge exactly once"
        );

        // The closing subscription ends the task: it sends FINISH and drops
        // its MonitorFinishGuard → the owner receives the terminal signal.
        let fin = tokio::time::timeout(Duration::from_secs(2), mon_fin_rx.recv())
            .await
            .expect("subscriber task must signal completion to the read-loop owner")
            .expect("completion channel stays open");
        assert_eq!(
            (fin.sid, fin.ioid, fin.op_id),
            (sid, ioid, op_id),
            "the signal identifies the exact op instance that ended"
        );

        // The task emitted a MONITOR FINISH (subcmd 0x10) before ending.
        let mut saw_finish = false;
        while let Ok(buf) = rx.try_recv() {
            if let Ok(Some((frame, _))) = try_parse_frame(&buf)
                && frame.header.command == Command::Monitor.code()
            {
                let mut cur = frame.cursor();
                let _ = cur.get_u32(order);
                if let Ok(sub) = cur.get_u8()
                    && sub & 0x10 != 0
                {
                    saw_finish = true;
                }
            }
        }
        assert!(saw_finish, "source close emits a MONITOR FINISH frame");

        // Owner applies the signal: op removed, terminal Executing→Idle.
        apply_monitor_finish(&mut channels, fin);
        assert!(
            !channels[&sid].ops.contains_key(&ioid),
            "owner removes the op from ch.ops on monitor finish"
        );
        assert_eq!(
            *starts.lock(),
            vec![true, false],
            "terminal notify_monitor_start(false) fires exactly once after START"
        );
    }

    // ================================================================
    // a data-phase GET/PUT error reply must echo the request's
    // data subcmd (`0x00` GET exec, `0x40` PUT readback), NOT the INIT
    // subcmd `0x08`. pvxs writes `op->subcmd` into EVERY reply
    // (`serverget.cpp:82-84`, recorded at `:475`) and emits a
    // status-only body on `!sts.isSuccess()` (`:84-94`). The pre-fix
    // `send_op_error` hardcoded `0x08`, so a client awaiting GET data
    // decoded the failure as a (malformed) INIT response and lost the
    // server status behind a phase mismatch.
    // ================================================================

    /// A source whose value read always fails (`get_value` → `None`),
    /// driving the GET exec / PUT readback data-phase task onto its
    /// `send_op_error` path. INIT still succeeds via `ch.introspection`.
    struct Bfr13FailSource;
    impl crate::server_native::source::ChannelSource for Bfr13FailSource {
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
        async fn get_value(&self, _: &str) -> Option<PvField> {
            None
        }
        async fn put_value(&self, _: &str, _: PvField) -> Result<(), OpError> {
            Ok(())
        }
        async fn is_writable(&self, _: &str) -> bool {
            true
        }
        async fn subscribe(&self, _: &str) -> Option<MonitorStream<PvField>> {
            None
        }
    }

    /// One channel `sid=1`/`dut` with the supplied prototype.
    fn bfr13_channels(intro: Option<FieldDesc>, source: DynSource) -> HashMap<u32, ChannelState> {
        let mut channels = HashMap::new();
        channels.insert(
            1,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid: 1,
                introspection: intro.map(std::sync::Arc::new),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );
        channels
    }

    /// Encode an INIT pvRequest body (empty struct → all-field mask).
    fn bfr13_init_pv_request(order: ByteOrder, payload: &mut Vec<u8>) {
        let req_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![],
        };
        let req_val = PvField::Structure(PvStructure::new(""));
        crate::pvdata::encode::encode_type_desc(&req_desc, order, payload);
        crate::pvdata::encode::encode_pv_field(&req_val, &req_desc, order, payload);
    }

    /// `(subcmd, status)` from a status-only op reply payload.
    fn bfr13_parse_reply(buf: &[u8], order: ByteOrder, expect_cmd: Command) -> (u8, Status) {
        let (frame, _) = try_parse_frame(buf)
            .expect("reply frame parses")
            .expect("complete frame");
        assert_eq!(
            frame.header.command,
            expect_cmd.code(),
            "reply command matches the request"
        );
        let mut cur = frame.cursor();
        let _ioid = cur.get_u32(order).expect("ioid");
        let subcmd = cur.get_u8().expect("subcmd");
        let status = Status::decode(&mut cur, order).expect("status");
        (subcmd, status)
    }

    /// a GET whose source read fails during the data phase
    /// replies with the request's data subcmd `0x00` and an error
    /// status — not an INIT `0x08` frame.
    #[epics_macros_rs::epics_test]
    async fn bfr13_get_data_phase_error_echoes_data_subcmd() {
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::tcp::ClientCredentials;

        let order = ByteOrder::Little;
        let (sid, ioid) = (1u32, 700u32);
        let source: DynSource = Arc::new(Bfr13FailSource);
        let mut channels = bfr13_channels(Some(three_field_intro()), source.clone());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // GET INIT (subcmd 0x08) — succeeds via ch.introspection.
        let mut init_payload = Vec::new();
        init_payload.put_u32(sid, order);
        init_payload.put_u32(ioid, order);
        init_payload.put_u8(0x08);
        bfr13_init_pv_request(order, &mut init_payload);
        let init_frame = synth_frame(Command::Get, order, init_payload);
        handle_op(
            &init_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Get,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("GET INIT ok");
        let _ = rx.recv().await.expect("GET INIT reply");

        // GET EXEC (subcmd 0x00) — get_value → None → send_op_error.
        let mut exec_payload = Vec::new();
        exec_payload.put_u32(sid, order);
        exec_payload.put_u32(ioid, order);
        exec_payload.put_u8(0x00);
        let exec_frame = synth_frame(Command::Get, order, exec_payload);
        handle_op(
            &exec_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Get,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("GET EXEC ok");
        let resp = rx.recv().await.expect("GET EXEC error reply emitted");

        let (subcmd, status) = bfr13_parse_reply(&resp, order, Command::Get);
        assert_eq!(
            subcmd, 0x00,
            "data-phase GET error must echo the request data subcmd 0x00, not INIT 0x08"
        );
        assert!(
            !status.is_success(),
            "data-phase GET failure carries an error status"
        );
    }

    /// build a two-channel connection (sid 1 and sid 2), each advertising
    /// `three_field_intro`, with no ops yet. Mirrors the connection-wide IOID
    /// scope of pvxs `ServerConn` (one `opByIOID` across all channels).
    fn two_channel_conn(source: DynSource) -> HashMap<u32, ChannelState> {
        let mut channels = HashMap::new();
        for sid in [1u32, 2u32] {
            channels.insert(
                sid,
                ChannelState {
                    name: format!("dut{sid}"),
                    cid: sid - 1,
                    sid,
                    introspection: Some(std::sync::Arc::new(three_field_intro())),
                    source: source.clone(),
                    stat: crate::server_native::peers::ChannelStat::new(String::new()),
                    open_cred: ClientCredentials::anonymous(TEST_PEER),
                    ops: HashMap::new(),
                },
            );
        }
        channels
    }

    /// An IOID already live on one channel makes an INIT reusing it on a
    /// *different* channel connection-fatal — pvxs scopes IOIDs to
    /// `ServerConn::opByIOID`, not per channel (serverget.cpp:378-384).
    #[epics_macros_rs::epics_test]
    async fn pva_conn_wide_ioid_init_duplicate_across_channels_is_fatal() {
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::tcp::ClientCredentials;

        let order = ByteOrder::Little;
        let ioid = 5u32;
        let source: DynSource = Arc::new(Bfr13FailSource);
        let mut channels = two_channel_conn(source.clone());
        // A live GET op on channel sid=1 reserves ioid=5 connection-wide.
        channels.get_mut(&1).unwrap().ops.insert(
            ioid,
            non_monitor_op_state(
                std::sync::Arc::new(three_field_intro()),
                OpKind::Get,
                BitSet::all_set(three_field_intro().total_bits()),
            ),
        );

        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // INIT a fresh GET on channel sid=2 reusing ioid=5.
        let mut init_payload = Vec::new();
        init_payload.put_u32(2, order);
        init_payload.put_u32(ioid, order);
        init_payload.put_u8(0x08);
        bfr13_init_pv_request(order, &mut init_payload);
        let init_frame = synth_frame(Command::Get, order, init_payload);
        let err = handle_op(
            &init_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Get,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect_err("reusing an IOID live on another channel is connection-fatal");
        assert!(
            matches!(err, PvaError::Decode(_)),
            "cross-channel duplicate INIT must reset the connection, got {err:?}"
        );
    }

    /// DESTROY_REQUEST is keyed on the connection-wide IOID: a DESTROY whose
    /// SID does not match the op's channel still destroys the op (pvxs
    /// serverconn.cpp:295-319 erases by IOID even when the channel-local erase
    /// fails). The pre-fix per-channel lookup would have leaked the op.
    #[epics_macros_rs::epics_test]
    async fn pva_conn_wide_destroy_request_routes_by_ioid_ignoring_sid() {
        let order = ByteOrder::Little;
        let ioid = 9u32;
        let source: DynSource = Arc::new(Bfr13FailSource);
        let mut channels = two_channel_conn(source.clone());
        channels.get_mut(&1).unwrap().ops.insert(
            ioid,
            non_monitor_op_state(
                std::sync::Arc::new(three_field_intro()),
                OpKind::Get,
                BitSet::all_set(three_field_intro().total_bits()),
            ),
        );

        // DESTROY addressed to the wrong SID (2) for an op owned by SID 1.
        let mut payload = Vec::new();
        payload.put_u32(2, order);
        payload.put_u32(ioid, order);
        let frame = synth_frame(Command::DestroyRequest, order, payload);
        handle_destroy_request(&frame, &mut channels).expect("DESTROY_REQUEST ok");

        assert!(
            !channels.get(&1).unwrap().ops.contains_key(&ioid),
            "DESTROY keyed by IOID must remove the op from its real channel \
             regardless of the supplied SID"
        );
    }

    /// a PUT readback (`subcmd & 0x40`) whose readback GET
    /// fails replies with the request's `0x40` subcmd and an error
    /// status — not INIT `0x08`.
    #[epics_macros_rs::epics_test]
    async fn bfr13_put_readback_error_echoes_0x40_subcmd() {
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::tcp::ClientCredentials;

        let order = ByteOrder::Little;
        let (sid, ioid) = (1u32, 701u32);
        let source: DynSource = Arc::new(Bfr13FailSource);
        let mut channels = bfr13_channels(Some(three_field_intro()), source.clone());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        // PUT INIT (subcmd 0x08).
        let mut init_payload = Vec::new();
        init_payload.put_u32(sid, order);
        init_payload.put_u32(ioid, order);
        init_payload.put_u8(0x08);
        bfr13_init_pv_request(order, &mut init_payload);
        let init_frame = synth_frame(Command::Put, order, init_payload);
        handle_op(
            &init_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Put,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("PUT INIT ok");
        let _ = rx.recv().await.expect("PUT INIT reply");

        // PUT readback EXEC (subcmd 0x40) — the readback GET reads the
        // current value via get_value_checked → None → send_op_error.
        let mut exec_payload = Vec::new();
        exec_payload.put_u32(sid, order);
        exec_payload.put_u32(ioid, order);
        exec_payload.put_u8(0x40);
        let exec_frame = synth_frame(Command::Put, order, exec_payload);
        handle_op(
            &exec_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Put,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("PUT readback EXEC ok");
        let resp = rx.recv().await.expect("PUT readback error reply emitted");

        let (subcmd, status) = bfr13_parse_reply(&resp, order, Command::Put);
        assert_eq!(
            subcmd, 0x40,
            "data-phase PUT readback error must echo the request subcmd 0x40, not INIT 0x08"
        );
        assert!(
            !status.is_success(),
            "PUT readback failure carries an error status"
        );
    }

    /// Boundary: an INIT-phase negotiation failure (here a
    /// missing prototype) must still echo the INIT subcmd `0x08`. The
    /// fix makes error replies echo the *request* subcmd uniformly, so
    /// an INIT request stays `0x08` while a data request becomes
    /// `0x00`/`0x40` — it does not flip every error to `0x00`.
    #[epics_macros_rs::epics_test]
    async fn bfr13_init_phase_error_still_echoes_0x08() {
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::tcp::ClientCredentials;

        let order = ByteOrder::Little;
        let (sid, ioid) = (1u32, 702u32);
        let source: DynSource = Arc::new(Bfr13FailSource);
        // No prototype on the channel → INIT fails "must provide prototype".
        let mut channels = bfr13_channels(None, source.clone());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        let mut init_payload = Vec::new();
        init_payload.put_u32(sid, order);
        init_payload.put_u32(ioid, order);
        init_payload.put_u8(0x08);
        let init_frame = synth_frame(Command::Get, order, init_payload);
        handle_op(
            &init_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Get,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("GET INIT handled");
        let resp = rx.recv().await.expect("INIT error reply emitted");

        let (subcmd, status) = bfr13_parse_reply(&resp, order, Command::Get);
        assert_eq!(
            subcmd, 0x08,
            "INIT-phase negotiation error must still echo the INIT subcmd 0x08"
        );
        assert!(
            !status.is_success(),
            "INIT negotiation failure carries an error status"
        );
    }

    /// Client side: a data-phase GET error reply is status-only
    /// (`ioid + subcmd + status`, no bitset/value). The client decode
    /// must surface it as `OpResponse::Status` so `op_get` reports the
    /// server status, instead of faulting on the missing value body
    /// (decode error) or mislabelling it "expected GET data, got
    /// Status". Mirrors the server fix above on the wire.
    #[test]
    fn bfr13_client_decodes_data_phase_error_as_status() {
        let order = ByteOrder::Little;
        let intro = three_field_intro();

        let mut payload = Vec::new();
        payload.put_u32(700u32, order); // ioid
        payload.put_u8(0x00); // data subcmd, as the fixed server now echoes
        Status::error("source read failed".to_string()).write_into(order, &mut payload);
        let frame = synth_frame(Command::Get, order, payload);

        match decode_op_response(&frame, Some(&intro))
            .expect("a status-only data-phase error must decode, not fault on a missing value")
        {
            OpResponse::Status(s) => {
                assert_eq!(
                    s.subcmd, 0x00,
                    "decoded status preserves the data-phase subcmd"
                );
                assert!(
                    !s.status.is_success(),
                    "the server's failure status is surfaced to the op"
                );
            }
            other => {
                panic!("data-phase GET error must decode to OpResponse::Status, got {other:?}")
            }
        }
    }

    /// Build a client CONNECTION_VALIDATION frame selecting method "ca"
    /// with an auth structure carrying the given `user`.
    fn build_ca_validation_frame(order: ByteOrder, user: &str) -> Frame {
        use crate::pvdata::{PvStructure, ScalarType, ScalarValue};
        let auth_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![("user".into(), FieldDesc::Scalar(ScalarType::String))],
        };
        let mut auth_struct = PvStructure::new("");
        auth_struct.fields.push((
            "user".into(),
            PvField::Scalar(ScalarValue::String(user.into())),
        ));
        let auth_val = PvField::Structure(auth_struct);

        let mut payload: Vec<u8> = Vec::new();
        payload.put_u32(0x10000, order);
        payload.put_u16(32_767, order);
        payload.put_u16(0, order);
        encode_string_into("ca", order, &mut payload);
        encode_type_desc(&auth_desc, order, &mut payload);
        encode_pv_field(&auth_val, &auth_desc, order, &mut payload);
        let header = PvaHeader::application(
            false,
            order,
            Command::ConnectionValidation.code(),
            payload.len() as u32,
        );
        Frame { header, payload }
    }

    /// Post-handshake CONNECTION_VALIDATION must re-run the full
    /// parse/commit/validated-reply sequence (pvxs keeps it in the live
    /// command switch, conn.cpp:247-260, and re-runs
    /// handle_CONNECTION_VALIDATION on each dispatch,
    /// serverconn.cpp:196-251). A second valid frame after a successful
    /// handshake must: emit a SECOND CONNECTION_VALIDATED, replace the
    /// connection credential, update the per-peer credential record, and
    /// re-fire the auth_complete hook with the new identity.
    #[epics_macros_rs::epics_test]
    async fn process_connection_validation_reauth_replaces_identity() {
        use std::sync::Mutex as StdMutex;

        let order = ByteOrder::Little;
        let observed: Arc<StdMutex<Vec<(String, String)>>> = Arc::new(StdMutex::new(Vec::new()));
        let observed_hook = observed.clone();
        let config = PvaServerConfig {
            auth_complete: Some(Arc::new(move |_peer, cred: &ClientCredentials| {
                observed_hook
                    .lock()
                    .unwrap()
                    .push((cred.method.clone(), cred.account.clone()));
            })),
            ..Default::default()
        };
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let peer_entry = crate::server_native::peers::PeerEntry::new(false);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let mut cred = ClientCredentials::anonymous(TEST_PEER);
        // One connection-scope inbound decode cache shared across both
        // validation frames, mirroring the read loop's single `rx_type_cache`.
        let mut decode_cache = TypeCache::new();

        // First (initial) handshake: identity becomes alice.
        let f1 = build_ca_validation_frame(order, "alice");
        process_connection_validation(
            &f1,
            &tx,
            order,
            false,
            &mut cred,
            peer,
            &peer_entry,
            &config,
            &mut decode_cache,
        )
        .await
        .expect("initial validation");
        assert_eq!(cred.account, "alice", "initial handshake commits alice");
        let v1 = rx.try_recv().expect("first CONNECTION_VALIDATED emitted");
        assert_eq!(
            v1[3],
            Command::ConnectionValidated.code(),
            "reply is CONNECTION_VALIDATED"
        );
        assert_eq!(
            *peer_entry.credentials.lock(),
            Some(("alice".to_string(), "ca".to_string())),
            "peer credential record set to alice"
        );

        // Second (post-handshake) re-auth: identity becomes bob.
        let f2 = build_ca_validation_frame(order, "bob");
        process_connection_validation(
            &f2,
            &tx,
            order,
            false,
            &mut cred,
            peer,
            &peer_entry,
            &config,
            &mut decode_cache,
        )
        .await
        .expect("re-auth validation");
        assert_eq!(
            cred.account, "bob",
            "post-handshake re-auth replaces the connection identity"
        );
        let v2 = rx.try_recv().expect("second CONNECTION_VALIDATED emitted");
        assert_eq!(
            v2[3],
            Command::ConnectionValidated.code(),
            "re-auth re-issues CONNECTION_VALIDATED"
        );
        assert_eq!(
            *peer_entry.credentials.lock(),
            Some(("bob".to_string(), "ca".to_string())),
            "peer credential record updated to bob"
        );

        // The hook fired once per frame, observing each committed identity
        // in order — proving the new identity is used downstream.
        assert_eq!(
            *observed.lock().unwrap(),
            vec![
                ("ca".to_string(), "alice".to_string()),
                ("ca".to_string(), "bob".to_string()),
            ],
            "auth_complete fires on every validation with the new identity"
        );
    }

    /// Build a client CONNECTION_VALIDATION frame selecting an arbitrary
    /// `method` and carrying a decodable auth structure with the given `user`.
    /// Used to drive an *unadvertised* method (e.g. `"bogus"`) past
    /// `parse_client_credentials` — which only short-circuits empty/"anonymous"
    /// and `ca`-without-user — so the candidate reaches the advertised gate.
    fn build_validation_frame_with_method(order: ByteOrder, method: &str, user: &str) -> Frame {
        use crate::pvdata::{PvStructure, ScalarType, ScalarValue};
        let auth_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![("user".into(), FieldDesc::Scalar(ScalarType::String))],
        };
        let mut auth_struct = PvStructure::new("");
        auth_struct.fields.push((
            "user".into(),
            PvField::Scalar(ScalarValue::String(user.into())),
        ));
        let auth_val = PvField::Structure(auth_struct);

        let mut payload: Vec<u8> = Vec::new();
        payload.put_u32(0x10000, order);
        payload.put_u16(32_767, order);
        payload.put_u16(0, order);
        encode_string_into(method, order, &mut payload);
        encode_type_desc(&auth_desc, order, &mut payload);
        encode_pv_field(&auth_val, &auth_desc, order, &mut payload);
        let header = PvaHeader::application(
            false,
            order,
            Command::ConnectionValidation.code(),
            payload.len() as u32,
        );
        Frame { header, payload }
    }

    /// A post-handshake CONNECTION_VALIDATION selecting an *unadvertised*
    /// method must reply Status::Error but leave the connection's effective
    /// credential at the previously committed identity — pvxs clones `cred`
    /// into a local `C`, commits `cred = C` only via an advertised method, and
    /// only then rejects an unadvertised `selected` (serverconn.cpp:221-241),
    /// so a rejected re-auth is a rejected credential *update*, not a logout.
    /// Pre-fix Rust committed the claim and then forced `*cred` to anonymous,
    /// stripping a live `alice/ca` connection's ACF identity.
    #[epics_macros_rs::epics_test]
    async fn process_connection_validation_unadvertised_reauth_keeps_previous_credential() {
        use std::io::Cursor;
        use std::sync::Mutex as StdMutex;

        let order = ByteOrder::Little;
        let observed: Arc<StdMutex<Vec<(String, String)>>> = Arc::new(StdMutex::new(Vec::new()));
        let observed_hook = observed.clone();
        let config = PvaServerConfig {
            auth_complete: Some(Arc::new(move |_peer, cred: &ClientCredentials| {
                observed_hook
                    .lock()
                    .unwrap()
                    .push((cred.method.clone(), cred.account.clone()));
            })),
            ..Default::default()
        };
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let peer_entry = crate::server_native::peers::PeerEntry::new(false);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let mut cred = ClientCredentials::anonymous(TEST_PEER);
        let mut decode_cache = TypeCache::new();

        // Initial handshake: identity becomes alice/ca.
        let f1 = build_ca_validation_frame(order, "alice");
        process_connection_validation(
            &f1,
            &tx,
            order,
            false,
            &mut cred,
            peer,
            &peer_entry,
            &config,
            &mut decode_cache,
        )
        .await
        .expect("initial validation");
        assert_eq!(cred.account, "alice", "initial handshake commits alice");
        assert_eq!(cred.method, "ca", "initial handshake commits ca");
        let _ = rx.try_recv().expect("first CONNECTION_VALIDATED emitted");

        // Post-handshake re-auth with an UNADVERTISED method ("bogus") and a
        // decodable auth value. parse_client_credentials returns Some(bogus/...)
        // (only empty/"anonymous" and ca-without-user yield None), so the
        // candidate reaches the advertised gate and is rejected there.
        let f2 = build_validation_frame_with_method(order, "bogus", "mallory");
        process_connection_validation(
            &f2,
            &tx,
            order,
            false,
            &mut cred,
            peer,
            &peer_entry,
            &config,
            &mut decode_cache,
        )
        .await
        .expect("re-auth validation completes (connection stays open)");

        // The reply is a SECOND CONNECTION_VALIDATED carrying Status::Error.
        let v2 = rx.try_recv().expect("second CONNECTION_VALIDATED emitted");
        assert_eq!(
            v2[3],
            Command::ConnectionValidated.code(),
            "rejected re-auth still re-issues CONNECTION_VALIDATED"
        );
        let status =
            Status::decode(&mut Cursor::new(&v2[PvaHeader::SIZE..]), order).expect("decode status");
        assert!(
            !status.is_success(),
            "unadvertised re-auth replies an error status, got {status:?}"
        );
        assert_eq!(
            status.message(),
            Some("Client selects unadvertised auth"),
            "error status carries the unadvertised-auth message"
        );

        // The connection credential is UNCHANGED — still alice/ca, never the
        // rejected "bogus/mallory" claim and never a forced downgrade to
        // anonymous.
        assert_eq!(
            cred.account, "alice",
            "rejected re-auth leaves the previous account in force"
        );
        assert_eq!(
            cred.method, "ca",
            "rejected re-auth leaves the previous method in force"
        );

        // The per-peer credential record (the ACF/report identity) is unchanged.
        assert_eq!(
            *peer_entry.credentials.lock(),
            Some(("alice".to_string(), "ca".to_string())),
            "peer credential record still alice/ca after rejected re-auth"
        );

        // The auth_complete hook — pvxs's `auth_complete`, the ACF integration
        // point — fired on both frames; the second firing observed alice/ca, not
        // anonymous, proving a following ACF-gated operation still sees alice/ca.
        assert_eq!(
            *observed.lock().unwrap(),
            vec![
                ("ca".to_string(), "alice".to_string()),
                ("ca".to_string(), "alice".to_string()),
            ],
            "hook re-fires with the previous identity on a rejected re-auth"
        );
    }
}

#[cfg(test)]
mod autoexec_tests {
    //! R10-34: `autoExec` is a pvxs CLIENT-side builder flag
    //! (`SubBuilder::autoExec`, `src/pvxs/client.h:698` → `op->autoExec`,
    //! `clientget.cpp:633`), never a pvRequest member. pvxs has NO
    //! server-side reader for it: `serverget.cpp:488-492` runs `onPut` on
    //! every `CMD_PUT` with `!init`, whatever the client's `autoExec`.
    //!
    //! The port used to parse `record._options.autoExec` server-side into a
    //! per-op flag. These tests pin the contract that replaces it: the
    //! option is INERT on the server. They fail if a server-side gate is
    //! reintroduced.

    use super::*;
    use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
    use crate::server_native::SharedSource;
    use crate::server_native::runtime::PvaServerConfig;
    use crate::server_native::shared_pv::SharedPV;
    use crate::server_native::tcp::ClientCredentials;
    use std::sync::Arc;

    // Local test scaffolding, per this file's convention for `#[cfg(test)]`
    // sub-modules (`mod tests` keeps its copies private).
    fn synth_frame(command: Command, order: ByteOrder, payload: Vec<u8>) -> Frame {
        let header = PvaHeader::application(false, order, command.code(), payload.len() as u32);
        Frame { header, payload }
    }

    fn discard_mon_fin() -> mpsc::UnboundedSender<MonitorFinished> {
        mpsc::unbounded_channel().0
    }

    fn discard_exec_fin() -> mpsc::UnboundedSender<ExecFinished> {
        mpsc::unbounded_channel().0
    }

    /// Drive a PUT INIT (carrying `pv_request`) + PUT EXEC (writing 2.5)
    /// through `handle_op`, and return the PV's value afterwards.
    async fn put_through(pv_request: PvField) -> Option<PvField> {
        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 77;

        let pv = SharedPV::build_mailbox();
        let intro = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let mut initial = PvStructure::new("epics:nt/NTScalar:1.0");
        initial
            .fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        pv.open(intro.clone(), PvField::Structure(initial)).unwrap();

        let shared = SharedSource::new();
        shared.add("dut", pv.clone());
        let source: DynSource = Arc::new(shared);

        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(intro.clone())),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        let req_desc = pv_request.descriptor();
        let mut init_payload = Vec::new();
        init_payload.put_u32(sid, order);
        init_payload.put_u32(ioid, order);
        init_payload.put_u8(0x08);
        crate::pvdata::encode::encode_type_desc(&req_desc, order, &mut init_payload);
        crate::pvdata::encode::encode_pv_field(&pv_request, &req_desc, order, &mut init_payload);
        let init_frame = synth_frame(Command::Put, order, init_payload);
        handle_op(
            &init_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Put,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("PUT INIT ok");
        let _ = rx.recv().await.expect("INIT resp");

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
            &exec_frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Put,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &discard_mon_fin(),
            &discard_exec_fin(),
        )
        .await
        .expect("PUT EXEC ok");
        let _ = rx.recv().await.expect("PUT EXEC response emitted");
        pv.current()
    }

    fn request_with_autoexec(autoexec: Option<&str>) -> PvField {
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

    fn value_of(pv: Option<PvField>) -> f64 {
        match pv {
            Some(PvField::Structure(s)) => match s.get_field("value") {
                Some(PvField::Scalar(ScalarValue::Double(v))) => *v,
                other => panic!("unexpected value member: {other:?}"),
            },
            other => panic!("unexpected PV shape: {other:?}"),
        }
    }

    /// `record._options.autoExec=false` must NOT suppress the write: pvxs
    /// never reads the option server-side, so the EXEC commits.
    #[epics_macros_rs::epics_test]
    async fn autoexec_false_still_commits_the_put() {
        let after = put_through(request_with_autoexec(Some("false"))).await;
        assert_eq!(
            value_of(after),
            2.5,
            "autoExec=false is a client-side flag; the server must still commit the EXEC"
        );
    }

    /// Negative control: with the option absent the EXEC commits too — so
    /// the assertion above is pinning "the option is inert", not merely
    /// "PUT works".
    #[epics_macros_rs::epics_test]
    async fn put_without_autoexec_commits_identically() {
        let after = put_through(request_with_autoexec(None)).await;
        assert_eq!(value_of(after), 2.5);
    }

    /// The option is inert for EVERY spelling — including the ones the
    /// deleted parser treated as false (`"no"`, `"0"`) and the ones it
    /// rejected outright (`"maybe"`). No server-side branch may depend on
    /// this text.
    #[epics_macros_rs::epics_test]
    async fn every_autoexec_spelling_is_inert() {
        for v in ["false", "FALSE", "no", "0", "true", "maybe"] {
            let after = put_through(request_with_autoexec(Some(v))).await;
            assert_eq!(
                value_of(after),
                2.5,
                "autoExec={v:?} must not gate the server-side write"
            );
        }
    }
}

#[cfg(test)]
mod r14_tests {
    //! Regression: source calls must not block the TCP read loop.
    //!
    //! A SlowGetSource delays `get_value` by 500 ms. After the fix, `handle_op`
    //! spawns the source call and returns in < 50 ms. On main (pre-fix), `handle_op`
    //! awaited the source call inline and took ≥ 500 ms, failing the timing assertion.

    use super::*;
    use crate::pvdata::{FieldDesc, PvField, ScalarValue};
    use std::collections::HashMap;
    use std::net::SocketAddr;

    fn synth_frame(command: Command, order: ByteOrder, payload: Vec<u8>) -> Frame {
        let header = PvaHeader::application(false, order, command.code(), payload.len() as u32);
        Frame { header, payload }
    }

    struct SlowGetSource;

    impl crate::server_native::source::ChannelSource for SlowGetSource {
        async fn list_pvs(&self) -> Vec<String> {
            vec!["slow".into()]
        }
        async fn has_pv(&self, name: &str) -> bool {
            name == "slow"
        }
        async fn get_introspection(&self, _name: &str) -> Option<FieldDesc> {
            Some(FieldDesc::Variant)
        }
        async fn get_value(&self, _name: &str) -> Option<PvField> {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            Some(PvField::Scalar(ScalarValue::Double(1.0)))
        }
        async fn put_value(&self, _name: &str, _value: PvField) -> Result<(), OpError> {
            Ok(())
        }
        async fn is_writable(&self, _name: &str) -> bool {
            false
        }
        async fn subscribe(
            &self,
            _name: &str,
        ) -> Option<crate::server_native::MonitorStream<PvField>> {
            None
        }
    }

    #[epics_macros_rs::epics_test]
    async fn pva_r14_source_calls_no_head_of_line_block() {
        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 100;
        let peer: SocketAddr = "127.0.0.1:9001".parse().unwrap();

        let source: DynSource = std::sync::Arc::new(SlowGetSource);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let cred = ClientCredentials::anonymous(TEST_PEER);

        let intro = FieldDesc::Variant;
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        let mut ops: HashMap<u32, OpState> = HashMap::new();
        ops.insert(
            ioid,
            non_monitor_op_state(
                std::sync::Arc::new(intro.clone()),
                OpKind::Get,
                crate::proto::BitSet::new(),
            ),
        );
        channels.insert(
            sid,
            ChannelState {
                name: "slow".into(),
                cid: 1,
                sid,
                introspection: Some(std::sync::Arc::new(intro)),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops,
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);

        // Build a GET EXEC frame (subcmd = 0x00, no INIT bit).
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x00); // exec
        let frame = synth_frame(Command::Get, order, payload);

        let t0 = tokio::time::Instant::now();
        // this is a GET op, which never installs a MONITOR finish
        // guard, so a throwaway completion sender with its receiver dropped
        // is sufficient. (`r14_tests` is a sibling module and cannot reach
        // `tests::discard_mon_fin`.)
        let (mon_fin_tx, _mon_fin_rx) = mpsc::unbounded_channel::<MonitorFinished>();
        let (exec_fin_tx, _exec_fin_rx) = mpsc::unbounded_channel::<ExecFinished>();
        handle_op(
            &frame,
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Get,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &mon_fin_tx,
            &exec_fin_tx,
        )
        .await
        .expect("GET EXEC ok");
        let elapsed = t0.elapsed();

        // handle_op must return before the 500 ms source delay completes.
        // Pre-fix: elapsed ≥ 500 ms (blocking .await inline). Post-fix: < 50 ms.
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "handle_op blocked for {:?}; source call was not spawned",
            elapsed
        );

        // The response still arrives eventually via the spawned task.
        let resp = rx.recv().await.expect("GET response received from spawn");
        assert!(
            resp.len() > PvaHeader::SIZE,
            "GET response frame must be non-empty"
        );
    }
}

#[cfg(test)]
mod bfr15_tests {
    //! Regression: a data-phase EXEC runs only when the op is `Idle`,
    //! flips it to `Executing`, and a *second* EXEC that arrives while the
    //! first task is in flight is IGNORED — the first task is NOT cancelled
    //! (pvxs `serverget.cpp:467-476` runs the EXEC only on `Idle`/sets
    //! `Executing`; `:511-514` logs + drops a second EXEC; `:112-116` returns
    //! the op to `Idle` when the original callback replies). Pre-fix Rust
    //! aborted the in-flight task on a second EXEC (GET/PUT pre-cleared
    //! `data_task_abort`; RPC/PUT_GET/PROCESS overwrote it), and no `Idle`
    //! return-after-completion existed.
    //!
    //! Tested by invariant boundary, not by narrative:
    //! - `Idle` → first EXEC accepted, op `Executing` (tests a/b set up).
    //! - `Executing` → second EXEC ignored, in-flight task untouched (a/b).
    //! - explicit DESTROY still aborts the in-flight task (c).
    //! - task completion returns the op to `Idle`, so a later re-EXEC is
    //!   accepted as a fresh exec (d).
    //! - a stale completion signal (op-instance id mismatch) is a no-op (e).

    use super::*;
    use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn synth_frame(command: Command, order: ByteOrder, payload: Vec<u8>) -> Frame {
        let header = PvaHeader::application(false, order, command.code(), payload.len() as u32);
        Frame { header, payload }
    }

    /// Fired when the source future is dropped. A blocked source future is
    /// dropped exactly when the spawned task is aborted, so a non-zero count
    /// distinguishes "task aborted" from "task still blocked" (a normal
    /// completion is impossible within a test because the block is a 1-hour
    /// sleep).
    struct DropSignal(Arc<AtomicUsize>);
    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A source whose `get_value` / `put_value` can be made to block on a
    /// 1-hour sleep, keeping the spawned data-phase task in flight while the
    /// test fires a second EXEC. Each call is counted; cancellation of a
    /// blocked call is observable via the `*_cancelled` counters.
    struct ExecBlockSource {
        get_calls: Arc<AtomicUsize>,
        put_calls: Arc<AtomicUsize>,
        get_cancelled: Arc<AtomicUsize>,
        put_cancelled: Arc<AtomicUsize>,
        block_get: bool,
        block_put: bool,
    }

    fn nt_scalar_desc() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        }
    }

    fn nt_scalar_value(v: f64) -> PvField {
        let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
        s.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(v))));
        PvField::Structure(s)
    }

    impl crate::server_native::source::ChannelSource for ExecBlockSource {
        async fn list_pvs(&self) -> Vec<String> {
            vec!["dut".into()]
        }
        async fn has_pv(&self, name: &str) -> bool {
            name == "dut"
        }
        async fn get_introspection(&self, _: &str) -> Option<FieldDesc> {
            Some(nt_scalar_desc())
        }
        async fn get_value(&self, _: &str) -> Option<PvField> {
            self.get_calls.fetch_add(1, Ordering::SeqCst);
            if self.block_get {
                let _d = DropSignal(self.get_cancelled.clone());
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
            Some(nt_scalar_value(1.0))
        }
        async fn put_value(&self, _: &str, _: PvField) -> Result<(), OpError> {
            self.put_calls.fetch_add(1, Ordering::SeqCst);
            if self.block_put {
                let _d = DropSignal(self.put_cancelled.clone());
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
            Ok(())
        }
        async fn is_writable(&self, _: &str) -> bool {
            true
        }
        async fn subscribe(&self, _: &str) -> Option<crate::server_native::MonitorStream<PvField>> {
            None
        }
    }

    /// Channel map with a single `Idle` op of `kind` bound to `ioid`.
    fn channels_with_op(
        sid: u32,
        ioid: u32,
        kind: OpKind,
        source: DynSource,
    ) -> HashMap<u32, ChannelState> {
        let intro = nt_scalar_desc();
        let mut ops: HashMap<u32, OpState> = HashMap::new();
        ops.insert(
            ioid,
            non_monitor_op_state(std::sync::Arc::new(intro.clone()), kind, BitSet::new()),
        );
        let mut channels = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 1,
                sid,
                introspection: Some(std::sync::Arc::new(intro)),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops,
            },
        );
        channels
    }

    fn get_exec_frame(sid: u32, ioid: u32, order: ByteOrder) -> Frame {
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x00); // EXEC, not last-request, not INIT
        synth_frame(Command::Get, order, payload)
    }

    // Used only by the three reactor-dependent tests gated below, so it
    // carries the same predicate — otherwise it is dead code feature-ON.
    #[cfg(not(feature = "rtems-exec-model"))]
    fn put_exec_frame(sid: u32, ioid: u32, order: ByteOrder) -> Frame {
        let intro = nt_scalar_desc();
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x00); // EXEC: subcmd & 0x40 == 0 → a real write
        let bs = BitSet::all_set(intro.total_bits());
        bs.write_into(order, &mut payload);
        crate::pvdata::encode::encode_pv_field(&nt_scalar_value(2.5), &intro, order, &mut payload);
        synth_frame(Command::Put, order, payload)
    }

    // Used only by the three reactor-dependent tests gated below, so it
    // carries the same predicate — otherwise it is dead code feature-ON.
    #[cfg(not(feature = "rtems-exec-model"))]
    fn destroy_frame(sid: u32, ioid: u32, order: ByteOrder) -> Frame {
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        synth_frame(Command::DestroyRequest, order, payload)
    }

    /// Poll `counter` until it reaches `want`, panicking after ~1 s. Used to
    /// wait for a spawned task to enter its (blocking) source call.
    // Used only by the three reactor-dependent tests gated below, so it
    // carries the same predicate — otherwise it is dead code feature-ON.
    #[cfg(not(feature = "rtems-exec-model"))]
    async fn wait_for(counter: &AtomicUsize, want: usize) {
        for _ in 0..200 {
            if counter.load(Ordering::SeqCst) >= want {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!(
            "counter never reached {want}; stuck at {}",
            counter.load(Ordering::SeqCst)
        );
    }

    fn op_exec_state(channels: &HashMap<u32, ChannelState>, sid: u32, ioid: u32) -> ExecState {
        channels
            .get(&sid)
            .expect("channel present")
            .ops
            .get(&ioid)
            .expect("op present")
            .exec_state
    }

    fn op_abort_armed(channels: &HashMap<u32, ChannelState>, sid: u32, ioid: u32) -> bool {
        channels
            .get(&sid)
            .expect("channel present")
            .ops
            .get(&ioid)
            .expect("op present")
            .data_task_abort
            .is_some()
    }

    /// (a) `Executing` boundary on GET: a second GET EXEC arriving while the
    /// first source read is in flight is ignored — it neither starts a second
    /// source read nor aborts the first.
    // Reactor-dependent: the mock `ChannelSource` this test stands up blocks
    // inside `get_value`/`put_value` with `tokio::time::sleep`, and under
    // `rtems-exec-model` the `runtime::task` seam drives that future on a
    // `cbMedium` executor worker, which has no tokio reactor — the fixture
    // panics with "there is no reactor running" before the assertion is
    // reached. The production path under test is backend-neutral; only the
    // way the fixture blocks is not.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[epics_macros_rs::epics_test]
    async fn bfr15_second_get_exec_while_executing_is_ignored_not_aborted() {
        let order = ByteOrder::Little;
        let (sid, ioid) = (1u32, 100u32);
        let peer: SocketAddr = "127.0.0.1:9101".parse().unwrap();
        let get_calls = Arc::new(AtomicUsize::new(0));
        let get_cancelled = Arc::new(AtomicUsize::new(0));
        let source: DynSource = Arc::new(ExecBlockSource {
            get_calls: get_calls.clone(),
            put_calls: Arc::new(AtomicUsize::new(0)),
            get_cancelled: get_cancelled.clone(),
            put_cancelled: Arc::new(AtomicUsize::new(0)),
            block_get: true,
            block_put: false,
        });
        let mut channels = channels_with_op(sid, ioid, OpKind::Get, source.clone());
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let cred = ClientCredentials::anonymous(TEST_PEER);
        let mon = mpsc::unbounded_channel::<MonitorFinished>().0;
        let (exec_tx, _exec_rx) = mpsc::unbounded_channel::<ExecFinished>();

        // First EXEC: op was Idle → accepted, now Executing, source read spawned.
        handle_op(
            &get_exec_frame(sid, ioid, order),
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Get,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &mon,
            &exec_tx,
        )
        .await
        .expect("first GET EXEC ok");
        wait_for(&get_calls, 1).await;
        assert_eq!(op_exec_state(&channels, sid, ioid), ExecState::Executing);
        assert!(op_abort_armed(&channels, sid, ioid));

        // Second EXEC while Executing: ignored.
        handle_op(
            &get_exec_frame(sid, ioid, order),
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Get,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &mon,
            &exec_tx,
        )
        .await
        .expect("second GET EXEC ok (ignored)");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(
            get_calls.load(Ordering::SeqCst),
            1,
            "second GET EXEC must not start a second source read"
        );
        assert_eq!(
            get_cancelled.load(Ordering::SeqCst),
            0,
            "second GET EXEC must not abort the in-flight first read"
        );
        assert_eq!(op_exec_state(&channels, sid, ioid), ExecState::Executing);
        assert!(op_abort_armed(&channels, sid, ioid));
    }

    /// (b) `Executing` boundary on PUT: a second PUT EXEC arriving while the
    /// first write is in flight is ignored — neither a second write starts nor
    /// the first is aborted.
    // Reactor-dependent: the mock `ChannelSource` this test stands up blocks
    // inside `get_value`/`put_value` with `tokio::time::sleep`, and under
    // `rtems-exec-model` the `runtime::task` seam drives that future on a
    // `cbMedium` executor worker, which has no tokio reactor — the fixture
    // panics with "there is no reactor running" before the assertion is
    // reached. The production path under test is backend-neutral; only the
    // way the fixture blocks is not.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[epics_macros_rs::epics_test]
    async fn bfr15_second_put_exec_while_executing_is_ignored_not_aborted() {
        let order = ByteOrder::Little;
        let (sid, ioid) = (1u32, 200u32);
        let peer: SocketAddr = "127.0.0.1:9102".parse().unwrap();
        let get_calls = Arc::new(AtomicUsize::new(0));
        let put_calls = Arc::new(AtomicUsize::new(0));
        let put_cancelled = Arc::new(AtomicUsize::new(0));
        // get_value returns the prior value fast (the delta-PUT read-merge);
        // put_value is what blocks, holding the op Executing.
        let source: DynSource = Arc::new(ExecBlockSource {
            get_calls: get_calls.clone(),
            put_calls: put_calls.clone(),
            get_cancelled: Arc::new(AtomicUsize::new(0)),
            put_cancelled: put_cancelled.clone(),
            block_get: false,
            block_put: true,
        });
        let mut channels = channels_with_op(sid, ioid, OpKind::Put, source.clone());
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let cred = ClientCredentials::anonymous(TEST_PEER);
        let mon = mpsc::unbounded_channel::<MonitorFinished>().0;
        let (exec_tx, _exec_rx) = mpsc::unbounded_channel::<ExecFinished>();

        handle_op(
            &put_exec_frame(sid, ioid, order),
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Put,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &mon,
            &exec_tx,
        )
        .await
        .expect("first PUT EXEC ok");
        wait_for(&put_calls, 1).await;
        assert_eq!(op_exec_state(&channels, sid, ioid), ExecState::Executing);
        assert!(op_abort_armed(&channels, sid, ioid));

        handle_op(
            &put_exec_frame(sid, ioid, order),
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Put,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &mon,
            &exec_tx,
        )
        .await
        .expect("second PUT EXEC ok (ignored)");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(
            put_calls.load(Ordering::SeqCst),
            1,
            "second PUT EXEC must not start a second write"
        );
        assert_eq!(
            put_cancelled.load(Ordering::SeqCst),
            0,
            "second PUT EXEC must not abort the in-flight first write"
        );
        assert_eq!(op_exec_state(&channels, sid, ioid), ExecState::Executing);
    }

    /// (c) An explicit DESTROY_REQUEST still aborts an in-flight EXEC task:
    /// removing the op drops its `AbortOnDrop` guard, cancelling the spawned
    /// read. (The `Executing` gate suppresses only an implicit re-EXEC, never
    /// an explicit teardown.)
    // Reactor-dependent: the mock `ChannelSource` this test stands up blocks
    // inside `get_value`/`put_value` with `tokio::time::sleep`, and under
    // `rtems-exec-model` the `runtime::task` seam drives that future on a
    // `cbMedium` executor worker, which has no tokio reactor — the fixture
    // panics with "there is no reactor running" before the assertion is
    // reached. The production path under test is backend-neutral; only the
    // way the fixture blocks is not.
    #[cfg(not(feature = "rtems-exec-model"))]
    #[epics_macros_rs::epics_test]
    async fn bfr15_destroy_request_aborts_in_flight_exec_task() {
        let order = ByteOrder::Little;
        let (sid, ioid) = (1u32, 300u32);
        let peer: SocketAddr = "127.0.0.1:9103".parse().unwrap();
        let get_calls = Arc::new(AtomicUsize::new(0));
        let get_cancelled = Arc::new(AtomicUsize::new(0));
        let source: DynSource = Arc::new(ExecBlockSource {
            get_calls: get_calls.clone(),
            put_calls: Arc::new(AtomicUsize::new(0)),
            get_cancelled: get_cancelled.clone(),
            put_cancelled: Arc::new(AtomicUsize::new(0)),
            block_get: true,
            block_put: false,
        });
        let mut channels = channels_with_op(sid, ioid, OpKind::Get, source.clone());
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let cred = ClientCredentials::anonymous(TEST_PEER);
        let mon = mpsc::unbounded_channel::<MonitorFinished>().0;
        let (exec_tx, _exec_rx) = mpsc::unbounded_channel::<ExecFinished>();

        handle_op(
            &get_exec_frame(sid, ioid, order),
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Get,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &mon,
            &exec_tx,
        )
        .await
        .expect("GET EXEC ok");
        wait_for(&get_calls, 1).await;
        assert_eq!(get_cancelled.load(Ordering::SeqCst), 0);
        assert!(op_abort_armed(&channels, sid, ioid));

        // DESTROY_REQUEST removes the op → drops AbortOnDrop → aborts the task.
        handle_destroy_request(&destroy_frame(sid, ioid, order), &mut channels)
            .expect("DESTROY_REQUEST ok");
        wait_for(&get_cancelled, 1).await;
        assert!(
            !channels.get(&sid).unwrap().ops.contains_key(&ioid),
            "DESTROY_REQUEST must remove the op"
        );
    }

    /// (d) A completed EXEC returns the op to `Idle` (through the read-loop
    /// owner via `ExecFinished`/`apply_exec_finish`), so a later explicit
    /// re-EXEC is accepted as a fresh exec and runs a second source read.
    #[epics_macros_rs::epics_test]
    async fn bfr15_completed_exec_returns_op_to_idle_and_allows_reexec() {
        let order = ByteOrder::Little;
        let (sid, ioid) = (1u32, 400u32);
        let peer: SocketAddr = "127.0.0.1:9104".parse().unwrap();
        let get_calls = Arc::new(AtomicUsize::new(0));
        let source: DynSource = Arc::new(ExecBlockSource {
            get_calls: get_calls.clone(),
            put_calls: Arc::new(AtomicUsize::new(0)),
            get_cancelled: Arc::new(AtomicUsize::new(0)),
            put_cancelled: Arc::new(AtomicUsize::new(0)),
            block_get: false, // completes immediately
            block_put: false,
        });
        let mut channels = channels_with_op(sid, ioid, OpKind::Get, source.clone());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let cred = ClientCredentials::anonymous(TEST_PEER);
        let mon = mpsc::unbounded_channel::<MonitorFinished>().0;
        let (exec_tx, mut exec_rx) = mpsc::unbounded_channel::<ExecFinished>();

        // First EXEC completes; the task sends its response, then its
        // ExecFinishGuard drops and signals the owner.
        handle_op(
            &get_exec_frame(sid, ioid, order),
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Get,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &mon,
            &exec_tx,
        )
        .await
        .expect("first GET EXEC ok");
        let _resp = rx.recv().await.expect("first GET response emitted");

        // Owner applies the completion → op back to Idle, abort guard cleared.
        let fin = exec_rx.recv().await.expect("ExecFinished signalled");
        apply_exec_finish(&mut channels, fin);
        assert_eq!(op_exec_state(&channels, sid, ioid), ExecState::Idle);
        assert!(!op_abort_armed(&channels, sid, ioid));
        assert_eq!(get_calls.load(Ordering::SeqCst), 1);

        // Re-EXEC against the now-Idle op is accepted and runs a fresh read.
        handle_op(
            &get_exec_frame(sid, ioid, order),
            &tx,
            &mut channels,
            order,
            &fixed_out_order(order),
            OpKind::Get,
            &config,
            &mut encode_cache,
            &mut TypeCache::new(),
            peer,
            &cred,
            &mon,
            &exec_tx,
        )
        .await
        .expect("re-EXEC ok");
        let _resp2 = rx.recv().await.expect("re-EXEC response emitted");
        assert_eq!(
            get_calls.load(Ordering::SeqCst),
            2,
            "re-EXEC after Idle return must run a fresh source read"
        );
    }

    /// (e) ABA boundary: a stale completion signal (op-instance id mismatch,
    /// e.g. the ioid was removed and re-INIT'd) must NOT flip the fresh op
    /// back to `Idle`. Only an exact `monitor_op_id` match applies.
    #[epics_macros_rs::epics_test]
    async fn bfr15_apply_exec_finish_ignores_stale_op_id() {
        let (sid, ioid) = (1u32, 500u32);
        let source: DynSource = Arc::new(crate::server_native::SharedSource::new());
        let mut channels = channels_with_op(sid, ioid, OpKind::Get, source.clone());
        // Drive the op to Executing and capture its instance id.
        let op_id =
            begin_exec(channels.get_mut(&sid).unwrap(), ioid).expect("Idle op accepts the exec");
        assert_eq!(op_exec_state(&channels, sid, ioid), ExecState::Executing);

        // Stale signal (id+1): no-op.
        apply_exec_finish(
            &mut channels,
            ExecFinished {
                sid,
                ioid,
                op_id: op_id.wrapping_add(1),
                success: true,
            },
        );
        assert_eq!(
            op_exec_state(&channels, sid, ioid),
            ExecState::Executing,
            "stale op-instance id must not return the op to Idle"
        );

        // Matching signal: applies. The op is not last_request, so it returns
        // to Idle regardless of reply success.
        apply_exec_finish(
            &mut channels,
            ExecFinished {
                sid,
                ioid,
                op_id,
                success: true,
            },
        );
        assert_eq!(op_exec_state(&channels, sid, ioid), ExecState::Idle);
    }

    /// pvxs `ServerGPR::doReply`
    /// (serverget.cpp:86-116) returns an executing GPR op to `Idle` on an ERROR
    /// reply WITHOUT cleanup — `lastRequest` stays sticky for a later EXEC — and
    /// cleans the op up only after a SUCCESSFUL last-request reply. The pre-fix
    /// owner removed a `last_request` op on completion regardless of reply
    /// status, freeing the IOID on a source error / descriptor mismatch /
    /// rejected PUT that pvxs would have kept alive. The completion signal now
    /// carries reply success; an error completion must preserve the Idle op and
    /// a later success completion must remove it.
    #[test]
    fn last_request_gpr_error_completion_keeps_op_idle_then_success_removes() {
        let (sid, ioid) = (3u32, 88u32);
        let source: DynSource = Arc::new(crate::server_native::SharedSource::new());
        let mut op = non_monitor_op_state(
            std::sync::Arc::new(FieldDesc::Variant),
            OpKind::Get,
            BitSet::new(),
        );
        op.exec_state = ExecState::Executing;
        op.last_request = true;
        let op_id = op.monitor_op_id;

        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        let mut ops = HashMap::new();
        ops.insert(ioid, op);
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(FieldDesc::Variant)),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops,
            },
        );

        // ERROR completion of the last-request GET: the op survives, returns to
        // Idle, and keeps its sticky last_request marker for a later EXEC.
        apply_exec_finish(
            &mut channels,
            ExecFinished {
                sid,
                ioid,
                op_id,
                success: false,
            },
        );
        {
            let op = &channels[&sid].ops[&ioid];
            assert_eq!(
                op.exec_state,
                ExecState::Idle,
                "an error reply returns the GPR op to Idle (serverget.cpp:89-90)"
            );
            assert!(
                op.last_request,
                "an error reply must NOT clear the sticky last_request marker"
            );
        }

        // Re-EXEC drives it back to Executing.
        let op_id2 = begin_exec(channels.get_mut(&sid).unwrap(), ioid)
            .expect("re-EXEC accepted after an error reply");

        // SUCCESS completion now cleans the last-request op up.
        apply_exec_finish(
            &mut channels,
            ExecFinished {
                sid,
                ioid,
                op_id: op_id2,
                success: true,
            },
        );
        assert!(
            !channels[&sid].ops.contains_key(&ioid),
            "a SUCCESSFUL last-request reply removes the op (serverget.cpp:111-114)"
        );
    }

    /// GET_FIELD is a `ServerIntrospect` one-shot (serverintrospect.cpp:47-49):
    /// it is removed on EVERY terminal reply — success OR error — unlike a GPR
    /// op. The success-gating from that fix must apply
    /// ONLY to GPR kinds; a naive `last_request && success` for all kinds would
    /// leak a failed introspection's reserved IOID. Lock the kind distinction:
    /// an error completion of a GET_FIELD op still frees it.
    #[test]
    fn get_field_one_shot_removed_on_error_completion() {
        let (sid, ioid) = (4u32, 90u32);
        let source: DynSource = Arc::new(crate::server_native::SharedSource::new());
        let mut op = non_monitor_op_state(
            std::sync::Arc::new(FieldDesc::Variant),
            OpKind::GetField,
            BitSet::with_capacity(0),
        );
        op.exec_state = ExecState::Executing;
        // The Rust slow path always reserves a GET_FIELD op as last_request.
        op.last_request = true;
        let op_id = op.monitor_op_id;

        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        let mut ops = HashMap::new();
        ops.insert(ioid, op);
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(std::sync::Arc::new(FieldDesc::Variant)),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                open_cred: ClientCredentials::anonymous(TEST_PEER),
                ops,
            },
        );

        // Error completion (success = false) of a one-shot GET_FIELD still
        // removes it — the success gate does not reach this kind.
        apply_exec_finish(
            &mut channels,
            ExecFinished {
                sid,
                ioid,
                op_id,
                success: false,
            },
        );
        assert!(
            !channels[&sid].ops.contains_key(&ioid),
            "GET_FIELD one-shot is removed on every terminal reply, even an error"
        );
    }
}

#[cfg(test)]
mod inbound_message_cap_tests {
    //! The inbound size cap: what `read_frame` does with a header that
    //! announces more than the configured ceiling, and what the default
    //! ceiling is when nobody configures one.
    use super::*;
    use crate::server_native::config::DEFAULT_MAX_MESSAGE_SIZE;

    /// An 8-byte client→server application header announcing `payload_length`
    /// and nothing else. The body is never sent: the point of the cap is that
    /// the server refuses before it would allocate for one.
    fn header_announcing(payload_length: u32) -> Vec<u8> {
        let h = PvaHeader::application(
            false, // client direction — a server's inbound frame
            ByteOrder::Little,
            Command::Get.code(),
            payload_length,
        );
        let mut out = Vec::new();
        h.write_into(&mut out);
        assert_eq!(out.len(), PvaHeader::SIZE);
        out
    }

    #[epics_macros_rs::epics_test]
    async fn the_default_ceiling_refuses_an_oversized_header_without_reading_a_body() {
        let wire = header_announcing(DEFAULT_MAX_MESSAGE_SIZE as u32 + 1);
        let mut reader = std::io::Cursor::new(wire);
        let mut rx_buf = Vec::new();
        let err = read_frame(
            &mut reader,
            &mut rx_buf,
            Duration::from_secs(1),
            Some(DEFAULT_MAX_MESSAGE_SIZE),
        )
        .await
        .expect_err("an over-cap header must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("exceeds max_message_size"),
            "refusal must name the cap it broke: {msg}"
        );
        // Only the header was ever buffered — the refusal happened before the
        // body could be read, which is the whole reason the cap is checked on
        // the header rather than after reassembly.
        assert_eq!(rx_buf.len(), PvaHeader::SIZE);
    }

    /// The comparison is `>`, not `>=`: a message exactly at the ceiling is
    /// admitted. An off-by-one here would refuse the largest legal message.
    #[epics_macros_rs::epics_test]
    async fn a_message_exactly_at_the_ceiling_is_admitted() {
        let cap = 16usize;
        let mut wire = header_announcing(cap as u32);
        wire.extend_from_slice(&[0u8; 16]);
        let mut reader = std::io::Cursor::new(wire);
        let mut rx_buf = Vec::new();
        let frame = read_frame(&mut reader, &mut rx_buf, Duration::from_secs(1), Some(cap))
            .await
            .expect("a message exactly at the cap must be admitted");
        assert_eq!(frame.payload.len(), cap);
    }

    #[epics_macros_rs::epics_test]
    async fn one_byte_over_the_ceiling_is_refused() {
        let cap = 16usize;
        let wire = header_announcing(cap as u32 + 1);
        let mut reader = std::io::Cursor::new(wire);
        let mut rx_buf = Vec::new();
        let err = read_frame(&mut reader, &mut rx_buf, Duration::from_secs(1), Some(cap))
            .await
            .expect_err("one byte over the cap must be refused");
        assert!(err.to_string().contains("exceeds max_message_size"));
    }

    /// `None` is still expressible and still means unbounded — the same
    /// header the default ceiling refuses is accepted for buffering when the
    /// deployment opted out.
    #[epics_macros_rs::epics_test]
    async fn an_explicit_none_still_means_unbounded() {
        let wire = header_announcing(DEFAULT_MAX_MESSAGE_SIZE as u32 + 1);
        let mut reader = std::io::Cursor::new(wire);
        let mut rx_buf = Vec::new();
        let err = read_frame(&mut reader, &mut rx_buf, Duration::from_secs(1), None)
            .await
            .expect_err("the stream ends after the header, so this cannot succeed");
        // The refusal must be the *stream ending*, not the cap.
        assert!(
            err.to_string().contains("client closed"),
            "an unbounded reader must not refuse on size: {err}"
        );
    }
}
