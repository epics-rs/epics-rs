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
use tokio::sync::{broadcast, mpsc};
use tokio::time::interval;
use tracing::{debug, error, info, warn};

use crate::client_native::decode::{Frame, PeerRole, try_parse_frame_role};
use crate::error::{PvaError, PvaResult};
use crate::proto::{
    BitSet, ByteOrder, Command, ControlCommand, HeaderFlags, MessageType, PVA_VERSION, PvaHeader,
    QosFlags, Status, WriteExt, encode_size_into, encode_string_into,
};
use crate::pvdata::encode::{
    EncodeTypeCache, TypeCache, decode_pv_field_cached, decode_pv_field_with_bitset_cached,
    decode_type_desc_cached, encode_pv_field, encode_type_desc, encode_type_desc_cached,
};
use crate::pvdata::{FieldDesc, PvField};

use super::runtime::PvaServerConfig;
use super::source::{DynSource, OpError};

static NEXT_SID: AtomicU32 = AtomicU32::new(1);
fn alloc_sid() -> u32 {
    NEXT_SID.fetch_add(1, Ordering::Relaxed)
}

// serverChannelID sentinel in a CREATE_CHANNEL failure reply. pvxs
// uses `sid = -1` (serverchan.cpp:273/338) and wires it as 0xFFFFFFFF;
// `NEXT_SID` starts at 1, so this value can never be a live channel id.
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

/// Render a monitor `_options` scalar value for a pvxs-shaped
/// `logRemote` message. pvxs streams the option `Value` through
/// `operator<<` (`SB()<<…<<pipeline`); the scalar text form is the
/// faithful approximation. Non-scalar fields render as `<non-scalar>`.
fn render_option_value(f: &PvField) -> String {
    match f {
        PvField::Scalar(sv) => sv.to_string(),
        _ => "<non-scalar>".to_string(),
    }
}

struct PipelineOptions {
    enabled: bool,
    queue_size: u32,
    /// The client-requested `record._options.queueSize`, recorded ONLY
    /// when it was present and valid (`>= 2`), independent of pipeline
    /// enablement. pvxs assigns `op->limit = qSize` for any valid
    /// `queueSize` outside the `if(op->pipeline)` block
    /// (`servermon.cpp:533-543`), so a non-pipeline monitor's queue depth
    /// is the requested value too. `None` means absent/invalid → the
    /// server keeps its configured default depth. `queue_size` above is
    /// the negotiated pipeline queue depth (`op->limit`, defaults to 4);
    /// it is NOT the initial credit window — that is the separate per-INIT
    /// `nack` rider (see [`parse_monitor_init_nack`]), which defaults to 0
    /// when absent. Both are separate from this squash-threshold override.
    requested_queue_size: Option<u32>,
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
    /// clean request. Regression R0604-PVASRV-MONITOR-OPTION-REMOTE-LOG-1.
    diagnostics: Vec<MonitorOptionDiag>,
}

/// Outcome of parsing a MONITOR INIT pvRequest's `record._options`
/// pipeline negotiation. Distinguishes the two cases the single `None`
/// return used to conflate — a parsed set of options vs. a negotiation
/// error the INIT must be rejected for.
enum MonitorPipelineRequest {
    /// Parsed options to apply (pipeline on or off).
    Options(PipelineOptions),
    /// pvxs `servermon.cpp:537-540`: `pipeline=true` with a PRESENT but
    /// invalid (`<2` or unparseable) `queueSize`. The pipeline
    /// sub-protocol requires agreement on `queueSize`, so the INIT is
    /// rejected with an error (`ctrl->error(...)` + `return`) rather
    /// than silently downgraded to a non-pipeline monitor.
    Reject,
}

/// pvxs `servermon.cpp:554-581` — derive the pipeline ACK-refill
/// threshold `ackAt` from `record._options.ackAny` and the negotiated
/// `queueSize` (pvxs `limit`). `ackAny` may be a plain integer (typed
/// builder scalar or a numeric string) or a percentage string
/// (`"N%"`). An absent or unparseable value keeps the pvxs default of
/// `1`; an explicit `0` becomes `queueSize / 2`; the result clamps to
/// `[1, queueSize]`. `queue_size` MUST be `>= 1` (the caller only
/// invokes this for an enabled pipeline, where `queueSize >= 2`).
///
/// Returns the effective `ackAt` plus an optional pvxs
/// `Level::Crit` [`MonitorOptionDiag`]. pvxs emits a Crit `logRemote`
/// for exactly two cases (`servermon.cpp:557-573`), and leaves `ackAt`
/// at its default in both:
/// * a NON-scalar `ackAny` — `as<int>` and `as<string>` both fail →
///   "Unable to parse …" (`:570-573`);
/// * a `"N%"` percentage string whose numeric part fails to parse →
///   "Unable to parse% …" (`:561-568`).
///
/// A plain scalar string that simply fails integer parse (e.g.
/// `"garbage"`) is SILENTLY ignored by pvxs — `as<string>` succeeds, the
/// value has no `%` suffix, so neither branch fires and no diagnostic is
/// emitted (`:560-569`). This faithfully differs from the review doc's
/// imprecise `ackAny=garbage` Crit example. Regression
/// R0604-PVASRV-MONITOR-OPTION-REMOTE-LOG-1.
fn ack_at_from(ack_any: Option<&PvField>, queue_size: u32) -> (u32, Option<MonitorOptionDiag>) {
    use crate::pvdata::ScalarValue;
    // pvxs `MonitorOp::ackAt` struct default.
    let mut ack_at: u32 = 1;
    let mut diag: Option<MonitorOptionDiag> = None;
    match ack_any {
        Some(PvField::Scalar(sv)) => match sv {
            ScalarValue::String(s) => {
                let s = s.as_str_lossy();
                if let Some(pct) = s.strip_suffix('%').filter(|p| !p.is_empty()) {
                    match pct.trim().parse::<f64>() {
                        Ok(percent) => {
                            // pvxs `servermon.cpp:563` historically
                            // computed `clamp(percent,0,100) * limit` with NO
                            // `/ 100`, so any percent >= 1% saturated to the full
                            // queue after the `[1, limit]` clamp below, defeating
                            // the percentage control. Divide by 100 so `"50%"` of a
                            // queue of 4 is 2 — honoring the documented percentage
                            // semantics. pvxs adopts the same fix.
                            ack_at = (percent.clamp(0.0, 100.0) / 100.0 * queue_size as f64) as u32;
                        }
                        Err(e) => {
                            // pvxs `servermon.cpp:566-568`: a `"N%"` string whose
                            // numeric prefix fails `parseTo<double>` is a Crit
                            // logRemote; `ackAt` stays at its default.
                            diag = Some(MonitorOptionDiag {
                                level: MessageType::Fatal,
                                message: format!(
                                    "Unable to parse% record._options.ackAny : {s} : {e}"
                                ),
                            });
                        }
                    }
                } else if let Ok(n) = s.trim().parse::<u32>() {
                    ack_at = n;
                }
                // else: a plain non-`%` string that fails integer parse —
                // pvxs leaves `ackAt` default with NO logRemote
                // (`servermon.cpp:560-569` only logs the `%` branch).
            }
            ScalarValue::Byte(i) => ack_at = u32::try_from(*i).unwrap_or(1),
            ScalarValue::UByte(i) => ack_at = u32::from(*i),
            ScalarValue::Short(i) => ack_at = u32::try_from(*i).unwrap_or(1),
            ScalarValue::UShort(i) => ack_at = u32::from(*i),
            ScalarValue::Int(i) => ack_at = u32::try_from(*i).unwrap_or(1),
            ScalarValue::UInt(i) => ack_at = *i,
            ScalarValue::Long(l) => ack_at = u32::try_from(*l).unwrap_or(1),
            ScalarValue::ULong(l) => ack_at = u32::try_from(*l).unwrap_or(1),
            _ => {}
        },
        Some(other) => {
            // pvxs `servermon.cpp:570-573`: a NON-scalar `ackAny` fails both
            // `as<int>` and `as<string>` → Crit logRemote; `ackAt` default.
            diag = Some(MonitorOptionDiag {
                level: MessageType::Fatal,
                message: format!(
                    "Unable to parse record._options.ackAny : {}",
                    render_option_value(other)
                ),
            });
        }
        None => {}
    }
    // servermon.cpp:577-581.
    if ack_at == 0 {
        ack_at = queue_size / 2;
    }
    (ack_at.clamp(1, queue_size), diag)
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
/// [`FilteredMonitorEvent`] shape so it can flow through the shared
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

    let val = pv_value_leaf_to_epics(value)?;
    Some(FilteredMonitorEvent::new(
        MonitorEvent {
            snapshot: Snapshot::new(val, 0, 0, SystemTime::UNIX_EPOCH),
            origin: 0,
        },
        EventMask::VALUE,
    ))
}

/// Extract the value leaf of a PVA monitor `PvField` as an
/// `EpicsValue`, looking through an NT-style structure's `value`
/// member. Scalars and scalar arrays are both carried; returns
/// `None` for shapes with no representable value leaf (the
/// `arr` filter needs the real array, not a scalar fallback).
fn pv_value_leaf_to_epics(f: &PvField) -> Option<epics_base_rs::types::EpicsValue> {
    use crate::pvdata::ScalarValue;
    use epics_base_rs::types::{EpicsValue, PvString};

    fn scalar(sv: &ScalarValue) -> Option<EpicsValue> {
        Some(match sv {
            ScalarValue::Double(d) => EpicsValue::Double(*d),
            ScalarValue::Float(v) => EpicsValue::Float(*v),
            ScalarValue::Int(i) => EpicsValue::Long(*i),
            ScalarValue::Long(l) => EpicsValue::Int64(*l),
            ScalarValue::ULong(u) => EpicsValue::UInt64(*u),
            ScalarValue::Short(s) => EpicsValue::Short(*s),
            ScalarValue::UByte(b) => EpicsValue::Char(*b),
            ScalarValue::String(s) => EpicsValue::String(s.clone()),
            _ => return None,
        })
    }
    fn array(items: &[ScalarValue]) -> Option<EpicsValue> {
        // Empty array — default to a Double array (the filter slice
        // of an empty array is still empty, so the element type is
        // irrelevant for correctness here).
        let first = items.first();
        Some(match first {
            Some(ScalarValue::Double(_)) | None => EpicsValue::DoubleArray(
                items
                    .iter()
                    .map(|s| match s {
                        ScalarValue::Double(d) => *d,
                        _ => 0.0,
                    })
                    .collect(),
            ),
            Some(ScalarValue::Float(_)) => EpicsValue::FloatArray(
                items
                    .iter()
                    .map(|s| match s {
                        ScalarValue::Float(v) => *v,
                        _ => 0.0,
                    })
                    .collect(),
            ),
            Some(ScalarValue::Int(_)) => EpicsValue::LongArray(
                items
                    .iter()
                    .map(|s| match s {
                        ScalarValue::Int(v) => *v,
                        _ => 0,
                    })
                    .collect(),
            ),
            Some(ScalarValue::Long(_)) => EpicsValue::Int64Array(
                items
                    .iter()
                    .map(|s| match s {
                        ScalarValue::Long(v) => *v,
                        _ => 0,
                    })
                    .collect(),
            ),
            Some(ScalarValue::Short(_)) => EpicsValue::ShortArray(
                items
                    .iter()
                    .map(|s| match s {
                        ScalarValue::Short(v) => *v,
                        _ => 0,
                    })
                    .collect(),
            ),
            Some(ScalarValue::String(_)) => EpicsValue::StringArray(
                items
                    .iter()
                    .map(|s| match s {
                        ScalarValue::String(v) => v.clone(),
                        _ => PvString::new(),
                    })
                    .collect(),
            ),
            // a PVA `ulong[]` monitor value must reach the
            // `arr` filter as `UInt64Array` (mirrors the `scalar`
            // helper's `ULong -> UInt64`); without this arm a filtered
            // `DBF_UINT64` waveform fell through to a scalar `Double`
            // and was emitted as an empty `ulong[]` payload.
            Some(ScalarValue::ULong(_)) => EpicsValue::UInt64Array(
                items
                    .iter()
                    .map(|s| match s {
                        ScalarValue::ULong(v) => *v,
                        _ => 0,
                    })
                    .collect(),
            ),
            Some(ScalarValue::UByte(_)) => EpicsValue::CharArray(
                items
                    .iter()
                    .map(|s| match s {
                        ScalarValue::UByte(v) => *v,
                        _ => 0,
                    })
                    .collect(),
            ),
            _ => return None,
        })
    }
    match f {
        PvField::Scalar(sv) => scalar(sv),
        PvField::ScalarArray(items) => array(items),
        // Wire-decoded arrays arrive as the refcount-shared typed
        // form; convert to the generic scalar vector so the `arr`
        // filter sees the real array regardless of which variant the
        // source produced.
        PvField::ScalarArrayTyped(t) => array(&t.to_scalar_values()),
        PvField::Structure(s) => s
            .fields
            .iter()
            .find_map(|(k, v)| (k == "value").then_some(v))
            .and_then(pv_value_leaf_to_epics),
        _ => None,
    }
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
    let new_leaf = epics_value_to_pv_field(transformed)?;
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

/// Convert an `EpicsValue` produced by a filter back into a PVA
/// value-leaf `PvField` (scalar or scalar array).
fn epics_value_to_pv_field(v: &epics_base_rs::types::EpicsValue) -> Option<PvField> {
    use crate::pvdata::ScalarValue;
    use epics_base_rs::types::EpicsValue;
    Some(match v {
        EpicsValue::Double(d) => PvField::Scalar(ScalarValue::Double(*d)),
        EpicsValue::Float(f) => PvField::Scalar(ScalarValue::Float(*f)),
        EpicsValue::Long(i) => PvField::Scalar(ScalarValue::Int(*i)),
        EpicsValue::Short(s) => PvField::Scalar(ScalarValue::Short(*s)),
        EpicsValue::Char(c) => PvField::Scalar(ScalarValue::UByte(*c)),
        EpicsValue::Enum(e) => PvField::Scalar(ScalarValue::Int(*e as i32)),
        // Transient NTEnum carrier never reaches a filter result (coerced in
        // base at the link-write boundary); serve its index like a DBF_ENUM.
        EpicsValue::EnumWithChoices { index, .. } => {
            PvField::Scalar(ScalarValue::Int(*index as i32))
        }
        EpicsValue::String(s) => PvField::Scalar(ScalarValue::String(s.clone())),
        EpicsValue::Int64(l) => PvField::Scalar(ScalarValue::Long(*l)),
        EpicsValue::UInt64(u) => PvField::Scalar(ScalarValue::ULong(*u)),
        EpicsValue::DoubleArray(a) => {
            PvField::ScalarArray(a.iter().map(|x| ScalarValue::Double(*x)).collect())
        }
        EpicsValue::FloatArray(a) => {
            PvField::ScalarArray(a.iter().map(|x| ScalarValue::Float(*x)).collect())
        }
        EpicsValue::LongArray(a) => {
            PvField::ScalarArray(a.iter().map(|x| ScalarValue::Int(*x)).collect())
        }
        EpicsValue::ShortArray(a) => {
            PvField::ScalarArray(a.iter().map(|x| ScalarValue::Short(*x)).collect())
        }
        EpicsValue::CharArray(a) => {
            PvField::ScalarArray(a.iter().map(|x| ScalarValue::UByte(*x)).collect())
        }
        EpicsValue::EnumArray(a) => {
            PvField::ScalarArray(a.iter().map(|x| ScalarValue::Int(*x as i32)).collect())
        }
        EpicsValue::StringArray(a) => {
            PvField::ScalarArray(a.iter().map(|x| ScalarValue::String(x.clone())).collect())
        }
        EpicsValue::Int64Array(a) => {
            PvField::ScalarArray(a.iter().map(|x| ScalarValue::Long(*x)).collect())
        }
        EpicsValue::UInt64Array(a) => {
            PvField::ScalarArray(a.iter().map(|x| ScalarValue::ULong(*x)).collect())
        }
    })
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
            PvField::Scalar(ScalarValue::String(s)) => {
                Some(s.as_str_lossy().trim().to_ascii_lowercase())
            }
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

/// Inspect a decoded pvRequest for `record._options.pipeline` and
/// `record._options.queueSize`. pvxs `Subscription` defaults to
/// `queueSize = 4` when pipeline is enabled; we follow.
///
/// Returns `None` only when there is no `record._options` structure to
/// negotiate (a plain monitor). A present `_options` yields
/// [`MonitorPipelineRequest::Options`], or [`MonitorPipelineRequest::Reject`]
/// when `pipeline=true` is paired with a PRESENT-but-invalid
/// `queueSize` (pvxs `servermon.cpp:537-540`).
fn monitor_pipeline_options(req: &PvField) -> Option<MonitorPipelineRequest> {
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
    // pvxs `ServerConn::logRemote()` diagnostics for PRESENT-but-invalid
    // options, accumulated alongside the effective options without
    // altering any negotiated value.
    let mut diagnostics: Vec<MonitorOptionDiag> = Vec::new();
    // pvxs `servermon.cpp:523-540` parses `pipeline` via
    // `Value::as(bool)` and `queueSize` via the analogous scalar
    // conversion. A pvxs client using the typed builder form
    // (`.record("pipeline", true).record("queueSize", N)`) sends a
    // BOOL/INT, not the parsed-from-`record[pipeline=true]` STRING.
    // Pre-fix Rust matched only the string form; the typed builder
    // produced a pvRequest Rust decoded as non-pipelined, dropping
    // flow control. Accept both shapes.
    //
    // The effective `enabled` mapping below is unchanged from before this
    // finding (every input maps to the same true/false it did): the only
    // addition is a pvxs `Level::Warn` diagnostic for a PRESENT pipeline
    // value pvxs's `pipeline.as(bool)` cannot parse (an unrecognized
    // string or a non-scalar). pvxs leaves pipeline disabled in that case
    // (`servermon.cpp:528-530`), which the unrecognized→`false` mapping
    // already does, so the diagnostic is purely additive.
    let pipeline_field = opt_s
        .fields
        .iter()
        .find_map(|(k, v)| (k == "pipeline").then_some(v));
    let enabled = match pipeline_field {
        None => false,
        Some(PvField::Scalar(ScalarValue::Boolean(b))) => *b,
        Some(PvField::Scalar(ScalarValue::Byte(i))) => *i != 0,
        Some(PvField::Scalar(ScalarValue::UByte(i))) => *i != 0,
        Some(PvField::Scalar(ScalarValue::Short(i))) => *i != 0,
        Some(PvField::Scalar(ScalarValue::UShort(i))) => *i != 0,
        Some(PvField::Scalar(ScalarValue::Int(i))) => *i != 0,
        Some(PvField::Scalar(ScalarValue::UInt(i))) => *i != 0,
        Some(PvField::Scalar(ScalarValue::Long(i))) => *i != 0,
        Some(PvField::Scalar(ScalarValue::ULong(i))) => *i != 0,
        // Float/Double pipeline fell to the pre-fix `_ => None` →
        // `false`; keep that exact effective value with no diagnostic
        // (this is an effective-value choice, out of scope for the
        // diagnostics finding — do not start warning here).
        Some(PvField::Scalar(ScalarValue::Float(_)))
        | Some(PvField::Scalar(ScalarValue::Double(_))) => false,
        Some(PvField::Scalar(ScalarValue::String(s))) => {
            match s.as_str_lossy().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => true,
                "false" | "0" | "no" => false,
                _ => {
                    // PRESENT but unrecognized string: pvxs `as(bool)`
                    // fails → Warn logRemote, pipeline left disabled.
                    diagnostics.push(MonitorOptionDiag {
                        level: MessageType::Warning,
                        message: format!(
                            "Unable to parse record._options.pipeline : {}",
                            s.as_str_lossy()
                        ),
                    });
                    false
                }
            }
        }
        Some(other) => {
            // PRESENT non-scalar: pvxs `as(bool)` fails → Warn, disabled.
            diagnostics.push(MonitorOptionDiag {
                level: MessageType::Warning,
                message: format!(
                    "Unable to parse record._options.pipeline : {}",
                    render_option_value(other)
                ),
            });
            false
        }
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
    let queue_size = queue_size_field.and_then(|v| match v {
        PvField::Scalar(ScalarValue::String(s)) => s.as_str_lossy().parse::<u32>().ok(),
        PvField::Scalar(ScalarValue::Byte(i)) => u32::try_from(*i).ok(),
        PvField::Scalar(ScalarValue::UByte(i)) => Some(u32::from(*i)),
        PvField::Scalar(ScalarValue::Short(i)) => u32::try_from(*i).ok(),
        PvField::Scalar(ScalarValue::UShort(i)) => Some(u32::from(*i)),
        PvField::Scalar(ScalarValue::Int(i)) => u32::try_from(*i).ok(),
        PvField::Scalar(ScalarValue::UInt(i)) => Some(*i),
        PvField::Scalar(ScalarValue::Long(l)) => u32::try_from(*l).ok(),
        PvField::Scalar(ScalarValue::ULong(l)) => u32::try_from(*l).ok(),
        _ => None,
    });
    // pvxs `servermon.cpp:554` — `record._options.ackAny`. Parsed only
    // for an enabled pipeline (`ackAt` is meaningless without one).
    let ack_any = opt_s
        .fields
        .iter()
        .find_map(|(k, v)| (k == "ackAny").then_some(v));
    // A valid (`>= 2`) explicit `queueSize` is the per-op queue-depth
    // override regardless of pipeline (pvxs `op->limit = qSize` sits
    // OUTSIDE the `if(op->pipeline)` block, servermon.cpp:533-543).
    let requested_queue_size = match queue_size {
        Some(n) if n >= 2 => Some(n),
        _ => None,
    };
    let opts = if enabled {
        match queue_size {
            // Valid: use the requested window (pvxs `op->limit = qSize`).
            Some(n) if n >= 2 => {
                let (ack_at, ack_diag) = ack_at_from(ack_any, n);
                diagnostics.extend(ack_diag);
                PipelineOptions {
                    enabled: true,
                    queue_size: n,
                    requested_queue_size,
                    ack_at,
                    diagnostics,
                }
            }
            // PRESENT but invalid (`<2` or unparseable): pvxs
            // `servermon.cpp:537-540` rejects the INIT — the pipeline
            // sub-protocol requires agreement on `queueSize`. Do NOT
            // downgrade to a non-pipeline monitor. pvxs answers this with
            // `ctrl->error(...)` (a per-op error), NOT a logRemote, and
            // returns BEFORE the `ackAny` block — so no diagnostics are
            // owed. `diagnostics` is provably empty here (a rejected
            // pipeline required a recognized `pipeline=true`, which emits
            // no Warn, and `ackAny` is never reached), so dropping it
            // matches pvxs.
            _ if queue_size_present => return Some(MonitorPipelineRequest::Reject),
            // ABSENT: pvxs keeps the default `limit` (4) and leaves
            // pipeline enabled.
            _ => {
                let (ack_at, ack_diag) = ack_at_from(ack_any, 4);
                diagnostics.extend(ack_diag);
                PipelineOptions {
                    enabled: true,
                    queue_size: 4,
                    requested_queue_size,
                    ack_at,
                    diagnostics,
                }
            }
        }
    } else {
        // Non-pipeline: a valid `queueSize` sets the queue depth
        // (pvxs sets `op->limit` regardless of pipeline); an invalid or
        // absent one keeps the default depth 4. The pipeline credit
        // window does not apply to a non-pipeline monitor, while
        // `requested_queue_size` carries the squash override to the send
        // loop.
        let queue_size = match queue_size {
            Some(n) if n >= 2 => n,
            _ => 4,
        };
        // pvxs `servermon.cpp:541-543`: a PRESENT-but-invalid `queueSize`
        // on a NON-pipeline monitor keeps the default depth and emits a
        // Warn logRemote ("Unable to use …"). (Under pipeline the same
        // case is the Reject above, not a Warn.) `requested_queue_size`
        // is `Some` only for a valid `>= 2` value, so
        // present + `None` == present-invalid.
        if queue_size_present && requested_queue_size.is_none() {
            let rendered = queue_size_field
                .map(render_option_value)
                .unwrap_or_default();
            diagnostics.push(MonitorOptionDiag {
                level: MessageType::Warning,
                message: format!("Unable to use record._options.queueSize : {rendered}"),
            });
        }
        PipelineOptions {
            enabled: false,
            queue_size,
            requested_queue_size,
            ack_at: 1,
            diagnostics,
        }
    };
    Some(MonitorPipelineRequest::Options(opts))
}

#[derive(Clone)]
#[allow(dead_code)]
struct ChannelState {
    name: String,
    cid: u32,
    sid: u32,
    introspection: Option<FieldDesc>,
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
    /// (`stat.add_tx`/`add_rx`) so [`crate::server_native::PvaServer::report`]
    /// can show per-channel byte counters (pvxs `chan->statTx/statRx`,
    /// server.cpp:260-268).
    stat: Arc<crate::server_native::peers::ChannelStat>,
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
            .field("ops", &self.ops)
            .finish()
    }
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

/// Apply the data-phase op continuation after a GET/PUT/RPC EXEC task
/// has been spawned.
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
/// The Rust EXEC handlers serialize their response from a spawned task that
/// cannot reach `ch.ops`, so the read-loop owner defers cleanup to
/// [`apply_exec_finish`], which fires when the task's [`ExecFinishGuard`]
/// signals completion (right after the reply is handed to the writer). Both
/// branches therefore keep the op reserved and install the abort guard; only
/// the bookkeeping differs:
///
/// - last request (`subcmd & 0x10`): mark `last_request` so the completion
///   owner removes the op (matching pvxs `ServerOp::cleanup`) once the reply
///   is out, not before.
/// - otherwise: leave it `Executing`; the completion owner returns it to
///   `Idle` so a later explicit re-EXEC is accepted.
fn finish_exec_data_task(
    ch: &mut ChannelState,
    ioid: u32,
    subcmd: u8,
    abort: tokio::task::AbortHandle,
) {
    if let Some(op_mut) = ch.ops.get_mut(&ioid) {
        if subcmd & QosFlags::DESTROY != 0 {
            op_mut.last_request = true;
        }
        op_mut.data_task_abort = Some(Arc::new(AbortOnDrop(abort)));
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
        // R0604-PVASRV-LASTREQUEST-ERROR-1: GPR cleanup is gated on a
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
    /// `record._options.autoExec` from the INIT pvRequest. pvxs
    /// uses this purely client-side to decide whether to send the
    /// PUT EXEC immediately after INIT or wait for an explicit
    /// `reExec()` call (clientget.cpp:123). The server has no
    /// queueing role — pvxs `serverget.cpp:488-492` calls `onPut`
    /// the moment a CMD_PUT with !init arrives, regardless of the
    /// client's autoExec setting. We keep the field for diagnostic
    /// echoing but DO NOT gate write commits on it.
    put_auto_exec: bool,
    /// full INIT pvRequest value (decoded). PVA PUT INIT
    /// carries per-operation options (`record._options.process` /
    /// `block`, etc.) that the data-phase payload does NOT carry.
    /// We stash the value here at INIT so the data-phase PUT can
    /// attach it to the [`ChannelContext`] forwarded to the source,
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
/// [`Self::acquire`] exactly once before sending.
struct MonitorPipelineCredit<'a> {
    /// `None` for a non-pipeline monitor — [`Self::acquire`] is then a
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
    /// Block until the pipeline window has a free slot, then consume one
    /// credit for the DATA frame about to be sent and fire the LOW
    /// watermark on the above→below crossing. A no-op for a non-pipeline
    /// monitor (`window` is `None`).
    ///
    /// Must be called exactly once per monitor DATA frame, AFTER the
    /// pause / filter gates (a held or filtered event produces no wire
    /// frame, so it must not consume a slot).
    async fn acquire(&self) {
        use std::sync::atomic::Ordering;
        let (Some(w), Some(n)) = (self.window, self.window_notify) else {
            return;
        };
        loop {
            let cur = w.load(Ordering::Relaxed);
            if cur > 0 {
                if w.compare_exchange(cur, cur - 1, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    break;
                }
                continue;
            }
            // Window exhausted — wait for an ACK to refill. `enable()`
            // registers the waiter eagerly so an ACK firing between the
            // recheck and the await is captured (`Notify::notified()`
            // does not register until first polled). Same pattern as
            // `channel.rs::wait_until_inactive`.
            let notified = n.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if w.load(Ordering::Relaxed) > 0 {
                continue;
            }
            notified.await;
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
/// On drop it fires [`WatermarkKind::Withdraw`] unconditionally for this
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

/// Drive a per-op MONITOR gate ([`crate::server_native::source::
/// MonitorGate`], supplied by the source on its `SubscriptionSeed`) from
/// this op's executing-state watch. Applies the current state
/// immediately on spawn — so a STOP that fired before this driver started
/// is not missed — then re-applies on every edge [`MonitorStartControl`]
/// publishes, coalescing to the latest executing state (idempotent
/// `set_active`, so net state always matches `executing`). Runs in its own
/// task so the hot monitor update loop is untouched; ends when the op tears
/// down and drops the watch sender (the backing subscriptions are then
/// removed with the aborted monitor — STOP=disable, teardown=remove).
fn spawn_monitor_gate_driver(
    gate: crate::server_native::source::MonitorGate,
    mut exec_rx: tokio::sync::watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let mut cur = *exec_rx.borrow_and_update();
        gate.set_active(cur).await;
        while exec_rx.changed().await.is_ok() {
            let desired = *exec_rx.borrow_and_update();
            if desired != cur {
                cur = desired;
                gate.set_active(desired).await;
            }
        }
    });
}

/// single owner of one MONITOR op's Executing<->Idle edge.
/// pvxs fires `MonitorControlOp::onStart(bool)` once when a monitor
/// begins producing and once when it stops (`servermon.cpp:677-683`); we
/// mirror that through [`ChannelSource::notify_monitor_start`], firing
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
/// [`PeerRegistry`](crate::server_native::PeerRegistry). lets [`crate::server_native::PvaServer::report`]
/// observe per-connection stats.
pub async fn run_tcp_server_with_peers(
    source: DynSource,
    bind_addr: SocketAddr,
    config: PvaServerConfig,
    peers: Arc<crate::server_native::peers::PeerRegistry>,
) -> PvaResult<()> {
    let listener = TcpListener::bind(bind_addr).await.map_err(PvaError::Io)?;
    // Standalone single-listener path (not driven by `PvaServer`): create
    // and wire the channel-invalidation broadcast here so a source served
    // this way — e.g. a PVA gateway — still force-disconnects downstream
    // channels on an operator `:drop`/`:flush`. `PvaServer`'s multi-listener
    // path creates it once in `run_pva_server` and shares the one sender
    // across every TCP/UDP listener; here there is a single listener, so a
    // local sender is sufficient. The initial receiver is dropped; the
    // sender stays alive for the accept loop's lifetime.
    let (channel_invalidator, _inv_rx0) =
        broadcast::channel::<String>(crate::server_native::runtime::CHANNEL_INVALIDATION_CAPACITY);
    source.set_channel_invalidator(channel_invalidator.clone());
    run_tcp_server_on_listener(source, listener, config, peers, channel_invalidator).await
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
    // Server-wide channel-invalidation broadcast. Each accepted connection
    // subscribes a receiver; a source publishes the PV name of any channel
    // that must be force-disconnected out of band (PVA gateway operator
    // `:drop`/`:flush`). See [`ChannelSource::set_channel_invalidator`].
    channel_invalidator: broadcast::Sender<String>,
) -> PvaResult<()> {
    let bind_addr = listener.local_addr().map_err(PvaError::Io)?;
    debug!(?bind_addr, "TCP listener up");
    let active = Arc::new(AtomicUsize::new(0));

    let tls_acceptor = config
        .tls
        .as_ref()
        .map(|cfg| tokio_rustls::TlsAcceptor::from(cfg.config.clone()));

    // track per-connection tasks in a JoinSet so they're
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
                // pvxs scopes `ignoreAddrs` to the UDP SEARCH admission
                // path (`Server::Pvt::onSearch`, server.cpp:654-670); the
                // TCP accept callback registers a `ServerConn` with no
                // ignore-list check (serverconn.cpp:461-467). Applying it
                // to TCP accepts here turned a discovery filter into a
                // transport ACL, blocking direct clients that reach the
                // endpoint via a name server / cached beacon / static
                // address. The UDP path keeps the filter (`filter_inbound`).
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
                let peers_for_task = peers.clone();
                let conn_invalidator = channel_invalidator.clone();
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

                    // TLS-NAMESERVER: peek the first byte to dispatch
                    // TLS vs plain PVA on a single port.
                    //
                    // TLS ClientHello record type = 0x16 — the TLS
                    // client sends this IMMEDIATELY after TCP connect
                    // (client-initiates). Plain PVA clients NEVER send
                    // a first byte; the server sends SET_BYTE_ORDER first.
                    //
                    // Dispatch rule (pvxs uses separate listeners per
                    // protocol via serverconn.h:193 `isTLS`; we unify):
                    //   peek Ok(1) && byte == 0x16 → TLS path
                    //   peek timeout (≤ 100 ms)    → plain PVA path
                    //   peek Ok(1) && byte != 0x16  → plain PVA path
                    //   peek Ok(0) / IO error       → drop (peer gone)
                    //
                    // 100 ms is enough for ClientHello to arrive (sent
                    // immediately by TLS stack) while adding negligible
                    // latency to plain PVA connections.
                    const PEEK_WINDOW: Duration = Duration::from_millis(100);
                    let is_tls_client = match acceptor.as_ref() {
                        None => false,
                        Some(_) => {
                            let mut b = [0u8; 1];
                            match tokio::time::timeout(PEEK_WINDOW, stream.peek(&mut b)).await {
                                Ok(Ok(1)) => b[0] == 0x16,
                                Ok(Ok(_)) => {
                                    debug!(?peer, "peer closed before first byte");
                                    active_dec.fetch_sub(1, Ordering::SeqCst);
                                    return;
                                }
                                Ok(Err(e)) => {
                                    debug!(?peer, "first-byte peek error: {e}");
                                    active_dec.fetch_sub(1, Ordering::SeqCst);
                                    return;
                                }
                                // Timeout → plain PVA client (server initiates).
                                Err(_) => false,
                            }
                        }
                    };

                    // register this connection in the peer registry
                    // so PvaServer::report() can surface it. Deferred to
                    // here (post-peek) so the `tls` flag reflects the
                    // actual protocol, not the server config.
                    let peer_entry = crate::server_native::peers::PeerEntry::new(is_tls_client);
                    peers_for_task.insert(peer, peer_entry.clone());

                    let result = match (acceptor, is_tls_client) {
                        // cap the TLS handshake — a peer
                        // that completes TCP but stalls during ClientHello
                        // would otherwise hold a `max_connections` slot
                        // until OS keepalive reaps it (~30s).
                        (Some(a), true) => {
                            match tokio::time::timeout(cfg.tls_handshake_timeout, a.accept(stream))
                                .await
                            {
                                Ok(Ok(tls_stream)) => {
                                    // derive the peer's x509 identity from
                                    // the *verified* certificate chain before
                                    // splitting the stream. rustls only
                                    // exposes `peer_certificates()` on the
                                    // whole `TlsStream`, and the chain has
                                    // already passed `WebPkiClientVerifier`,
                                    // so this is the cryptographically-checked
                                    // identity (pvxs `fill_credentials`).
                                    //
                                    // use trust_roots so that `authority`
                                    // is populated even when the peer sends a
                                    // partial chain (leaf-only or leaf+CA),
                                    // matching pvxs SSL_get0_verified_chain.
                                    let x509_id = {
                                        let (_, conn) = tls_stream.get_ref();
                                        let roots =
                                            cfg.tls.as_ref().map(|t| t.trust_roots.as_ref());
                                        conn.peer_certificates().and_then(|chain| match roots {
                                            Some(r) => {
                                                crate::auth::x509_credentials_from_chain_with_roots(
                                                    chain, r,
                                                )
                                            }
                                            None => crate::auth::x509_credentials_from_chain(chain),
                                        })
                                    };
                                    let (r, w) = tokio::io::split(tls_stream);
                                    handle_connection_io(
                                        src,
                                        Box::new(r),
                                        Box::new(w),
                                        peer,
                                        cfg,
                                        ConnInit {
                                            peer_entry: peer_entry.clone(),
                                            x509_identity: x509_id,
                                            channel_invalidator: conn_invalidator,
                                        },
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
                        _ => {
                            // Plain PVA: no TLS configured, or client sent
                            // non-TLS bytes (name-server, plain pvxs peer).
                            let (r, w) = stream.into_split();
                            handle_connection_io(
                                src,
                                Box::new(r),
                                Box::new(w),
                                peer,
                                cfg,
                                ConnInit {
                                    peer_entry: peer_entry.clone(),
                                    x509_identity: None,
                                    channel_invalidator: conn_invalidator,
                                },
                            )
                            .await
                        }
                    };
                    if let Err(e) = result {
                        debug!(?peer, "connection ended: {e}");
                    }
                    active_dec.fetch_sub(1, Ordering::SeqCst);
                    // drop the per-peer entry whether the
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
    /// Group / role memberships of [`Self::account`], re-derived
    /// SERVER-SIDE from the local passwd/group DB by
    /// [`Self::with_server_roles`] (pvxs `ClientCredentials::roles()` →
    /// `osdGetRoles`, serverconn.cpp:33-37). ACF rules of the form
    /// `R member group:operators` match against this set.
    ///
    /// SECURITY: this is NEVER populated from the wire. A client
    /// advertises a `groups`/`roles` field in CONNECTION_VALIDATION, but
    /// trusting it would let `account="nobody", roles=["admin"]` satisfy
    /// any group-gated rule — an ACL bypass. Every constructor funnels
    /// through `with_server_roles`, so a wire value can never reach here.
    pub roles: Vec<String>,
}

impl ClientCredentials {
    /// Re-derive [`Self::roles`] server-side from [`Self::account`]
    /// against the local passwd/group DB (pvxs `ClientCredentials::roles`
    /// → `osdGetRoles`). The single funnel every constructor / parse path
    /// passes through, so `roles` is server-derived by construction and a
    /// wire-advertised value can never reach the ACF gate.
    fn with_server_roles(mut self) -> Self {
        self.roles = crate::auth::osd_get_roles(&self.account);
        self
    }

    fn anonymous() -> Self {
        Self {
            method: "anonymous".into(),
            account: "anonymous".into(),
            host: String::new(),
            authority: String::new(),
            roles: Vec::new(),
        }
        .with_server_roles()
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
        .with_server_roles()
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
    if x509_locked {
        // a decode fault here is still fatal —
        // log + propagate. Pre-fix swallowed; pvxs
        // `serverconn.cpp:211-216` calls `bev.reset()`.
        match parse_client_credentials(frame, decode_cache)? {
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
        // a decode fault is now connection-fatal
        // (matches pvxs `serverconn.cpp:211-216`
        // bev.reset). An anonymous handshake (empty
        // method) returns Ok(None) and keeps the
        // existing anonymous credential. Only a fully
        // decoded auth structure replaces `cred`.
        if let Some(claimed) = parse_client_credentials(frame, decode_cache)? {
            *cred = claimed;
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
    // an mTLS connection is authenticated by its
    // verified certificate chain — `cred.method` is
    // `"x509"` regardless of the CONNECTION_VALIDATION
    // claim, and that is always a valid method when TLS is
    // in use (pvxs advertises `x509` for TLS transports).
    // So the unadvertised-method rejection only applies to
    // the plain-TCP `ca`/`anonymous` negotiation.
    let advertised = x509_locked || ADVERTISED_AUTH_METHODS.iter().any(|m| *m == cred.method);
    let validated_status = if advertised {
        Status::ok()
    } else {
        debug!(
            ?peer,
            method = %cred.method,
            "PVA client selects unadvertised auth method — replying Status::Error"
        );
        // the client picked an auth method the
        // server never advertised. The handshake completes
        // (pvxs keeps the connection open) but the claimed
        // credential MUST NOT survive — the server is about
        // to return Status::Error rejecting it. Leaving
        // `cred` as the unadvertised claim would let the
        // `auth_complete` hook and every later ACF-gated
        // operation see an identity the server just
        // rejected: a legacy rule without a METHOD(...)
        // clause would still match the claimed account.
        // Revert to anonymous so the rejected claim never
        // becomes the connection identity.
        *cred = ClientCredentials::anonymous();
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

fn parse_client_credentials(
    frame: &Frame,
    decode_cache: &mut TypeCache,
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
    let pos = cur.position();
    let peek = cur
        .get_u8()
        .map_err(|e| PvaError::Decode(format!("CONN_VALIDATION auth desc peek: {e}")))?;
    // A leading `0xFF` is the pvxs "null type" tag: the auth Value carries
    // no `user`/`host` fields. pvxs `serverconn.cpp:223-231` only sets
    // method/account inside the `auth["user"]` callback, so a null body
    // leaves the placeholder `anonymous/anonymous` exactly as a structure
    // with no `user` field does. Rather than return early here — which left
    // a null-auth "ca" handshake as `method="ca", account=""` and skipped
    // the shared ca-requires-user rule below — fall through with an empty
    // account so both the null and empty-structure paths take one rule.
    if peek != 0xFF {
        // Rewind and decode the real descriptor.
        cur.set_position(pos);
        // Route through the connection-scope decode cache so a client that
        // advertises its auth structure with a `0xFD <slot>` define here can
        // later reference it by `0xFE <slot>` from a pvRequest/EXEC body, and
        // vice versa — pvxs shares one connection `rxRegistry` (conn.h:23)
        // across every inbound decode, including CONNECTION_VALIDATION.
        let desc = decode_type_desc_cached(&mut cur, order, decode_cache)
            .map_err(|e| PvaError::Decode(format!("CONN_VALIDATION auth desc: {e}")))?;
        let value = decode_pv_field_cached(&desc, &mut cur, order, decode_cache)
            .map_err(|e| PvaError::Decode(format!("CONN_VALIDATION auth value: {e}")))?;
        if let PvField::Structure(s) = value {
            for (name, field) in &s.fields {
                match (name.as_str(), field) {
                    ("user", PvField::Scalar(crate::pvdata::ScalarValue::String(v))) => {
                        creds.account = v.as_str_lossy().into_owned();
                    }
                    ("host", PvField::Scalar(crate::pvdata::ScalarValue::String(v))) => {
                        creds.host = v.as_str_lossy().into_owned();
                    }
                    // A `groups`/`roles` field MAY be advertised here, but the
                    // server MUST NOT trust it (ACL bypass — see the `roles`
                    // field doc). pvxs reads only `user`/`host` off the wire
                    // and re-derives roles via `osdGetRoles`. We ignore it and
                    // re-derive in `with_server_roles` before returning.
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
    // an unadvertised method, reverting the identity to anonymous.
    if creds.method == "ca" && creds.account.is_empty() {
        return Ok(None);
    }
    // Roles are re-derived server-side from `account` (pvxs
    // `ClientCredentials::roles()`); any wire-advertised `groups`/`roles`
    // was ignored above.
    Ok(Some(creds.with_server_roles()))
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
    intro: Option<FieldDesc>,
    owner: DynSource,
}
/// Sender half of the CREATE_CHANNEL completion channel.
type CcTx = mpsc::Sender<CreateChannelCompletion>;

/// Per-connection context the accept loop establishes before handing the
/// split stream to [`handle_connection_io`]: the peer's report entry, the
/// verified mTLS identity (if any), and the server-wide channel-invalidation
/// sender. Bundled to keep the IO handler's argument count within budget.
struct ConnInit {
    peer_entry: Arc<crate::server_native::peers::PeerEntry>,
    /// x509 identity from the verified TLS peer chain, when this connection
    /// arrived over mutually-authenticated TLS. `None` for plain TCP or TLS
    /// without a client cert. When present it is the authoritative identity
    /// and overrides the CONNECTION_VALIDATION claim — mirrors pvxs
    /// `SSLContext::fill_credentials`.
    x509_identity: Option<crate::auth::X509Credentials>,
    /// Server-wide channel-invalidation broadcast (see
    /// [`ChannelSource::set_channel_invalidator`]). Subscribed once below; a
    /// published PV name force-disconnects every channel this connection
    /// currently serves under that name with a server-initiated
    /// DESTROY_CHANNEL.
    channel_invalidator: broadcast::Sender<String>,
}

async fn handle_connection_io(
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
    // abort the writer + heartbeat tasks the moment the read
    // loop returns. Without this, both linger up to `idle_timeout`
    // (default 45s) emitting ECHOes into a channel nobody is reading
    // and holding the writer half of the (now-disconnected) socket.
    // pvxs uses libevent-driven cleanup that shuts everything in one
    // pass; we rely on tokio JoinHandle::abort() via AbortOnDrop.
    let _writer_guard = AbortOnDrop(writer_task.abort_handle());

    // Track per-connection liveness for the idle-timeout watchdog and the
    // server-side echo heartbeat task.
    let last_rx = Arc::new(AtomicU64::new(now_nanos()));

    // Outbound byte order as a shared, mutable per-connection cell, seeded
    // from the configured wire order. The read loop latches a new value if
    // the peer sends a SET_BYTE_ORDER control frame mid-stream (pvxs
    // conn.cpp:169-188 `sendBE`; old pvAccess accepts it from either peer
    // at any time). `true` = Big. Single writer (the read loop, which also
    // keeps an in-sync local `order` for the synchronous dispatch path it
    // owns); one reader (the heartbeat task).
    let out_order = Arc::new(std::sync::atomic::AtomicBool::new(
        config.wire_byte_order.is_big(),
    ));

    // Spawn server-side heartbeat: send ECHO_REQUEST every 15 s; close if
    // we've been idle for `idle_timeout`.
    let last_rx_hb = last_rx.clone();
    let tx_hb = tx.clone();
    let out_order_hb = out_order.clone();
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
            // Read the current outbound order — the peer may have
            // re-negotiated it via a mid-stream SET_BYTE_ORDER
            // (pvxs conn.cpp:169-188).
            let order_hb = if out_order_hb.load(Ordering::Relaxed) {
                ByteOrder::Big
            } else {
                ByteOrder::Little
            };
            let h = PvaHeader::control(true, order_hb, ControlCommand::EchoRequest.code(), 0);
            let mut buf = Vec::with_capacity(8);
            h.write_into(&mut buf);
            if tx_hb.send(buf).await.is_err() {
                break;
            }
        }
    });
    let _hb_guard = AbortOnDrop(hb_handle.abort_handle());

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
    // Receiver on the server-wide channel-invalidation broadcast. A source
    // publishes the PV name of a channel that must be force-disconnected
    // out of band (PVA gateway operator `:drop`/`:flush`); the read-loop
    // arm below force-disconnects every channel in `channels` serving that
    // name with a server-initiated DESTROY_CHANNEL. Subscribing here (post-
    // accept, pre-handshake) is sufficient: no channel exists until after
    // CONNECTION_VALIDATION, so any invalidation during the handshake names
    // a PV this connection cannot yet be serving and is harmlessly missed.
    let mut inv_rx = channel_invalidator.subscribe();
    // Disables the invalidation select! arm once every sender has dropped
    // (server shutdown). Without the guard a closed `broadcast::Receiver`
    // resolves `recv()` immediately and forever, busy-looping the select.
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
        Some(id) => ClientCredentials::x509(id),
        None => ClientCredentials::anonymous(),
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
                        reply_stat = Some(stat.clone());
                        channels.insert(cc.sid, ChannelState {
                            name: cc.name,
                            cid: cc.cid,
                            sid: cc.sid,
                            introspection: resolved.intro,
                            source: resolved.owner,
                            stat: stat.clone(),
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
                            let ctx = channel_lifecycle_ctx(peer, &cred);
                            ch.source.notify_channel_open(&ch.name, &ctx);
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
                // A source invalidated a channel out of band (PVA gateway
                // operator `<prefix>:drop` / `:flush`). Force-disconnect
                // every channel this connection currently serves under the
                // published PV name with a server-initiated DESTROY_CHANNEL
                // — the downstream effect of pva2pva dropping a
                // `ChannelCacheEntry`: `channel->destroy()` →
                // `channelStateChange(DESTROYED)` fanout to every interested
                // `GWChannel` (chancache.cpp:76-99, server.cpp:133-135).
                match inv_res {
                    Ok(pv) => {
                        // Tear down every channel this connection serves
                        // under the published name through the single
                        // teardown owner. A channel hosts every op under one
                        // name, so destroying it ends that name's GET/PUT/
                        // MONITOR alike — matching pva2pva's per-channel,
                        // not per-pvRequest, destroy granularity.
                        let teardown = ChannelTeardownCtx {
                            tx: &tx,
                            order,
                            peer,
                            cred: &cred,
                            peer_entry: &peer_entry,
                        };
                        invalidate_named_channels(&pv, &mut channels, &teardown).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // The invalidation stream overflowed this
                        // connection's buffer (a very large `:flush` while
                        // this task was busy in its read loop). Some names
                        // were dropped before this task could act; the
                        // operator can re-issue. Log loudly — silently
                        // missing a force-disconnect would leave a channel
                        // bound to a dropped cache entry, the exact defect
                        // this path closes.
                        warn!(
                            ?peer,
                            dropped = n,
                            "channel-invalidation receiver lagged; some force-disconnects missed, re-issue :drop/:flush"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Every sender dropped (server shutting down). Stop
                        // polling this arm so it cannot busy-loop; this
                        // connection keeps serving until its own teardown.
                        inv_closed = true;
                    }
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
        last_rx.store(now_nanos(), Ordering::SeqCst);
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
        // Cap reassembly when an opt-in `max_message_size` is set.
        // read_frame already enforces it per-frame; without this an
        // adversary streams SegFirst → SegMiddle … forever, growing
        // seg_buf without bound. `None` = unbounded (pvxs
        // parity), so this guard only fires for hardened deployments
        // that opted into a ceiling.
        if let Some(cap) = max_msg_size {
            if seg_buf.len().saturating_add(frame.payload.len()) > cap {
                return Err(PvaError::Protocol(format!(
                    "segmented PVA message exceeds max_message_size ({} > {})",
                    seg_buf.len() + frame.payload.len(),
                    cap
                )));
            }
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
                    peer,
                    cred: &cred,
                    peer_entry: &peer_entry,
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
        close_channel(ch, peer, &cred);
    }
    conn_result
}

/// Decode the VALUE body of an INIT pvRequest, after its descriptor
/// has already been read from `cur`.
///
/// Distinguishes an ABSENT value body (the cursor is exhausted after
/// the descriptor) from a PRESENT but malformed one. pvxs
/// `from_wire_type_value` requires type+value and resets the connection
/// on `!M.good()` (`serverget.cpp:368-371`, `servermon.cpp:489`). We
/// tolerate the absent body for future-compat / legacy clients, but a
/// present-but-undecodable
/// body is an INIT protocol error — the previous `decode_pv_field(..).ok()`
/// collapsed both into `None`, so a malformed pvRequest silently
/// dropped its `_filter` / pipeline / `process`|`block` options and the
/// op was registered with an OK INIT.
///
/// Returns `Ok(None)` for an absent body, `Ok(Some(value))` for a
/// present-and-decoded one, and `Err(message)` for a present-but-
/// malformed one (the caller turns that into an INIT error).
fn decode_init_pv_request_value(
    cur: &mut std::io::Cursor<&[u8]>,
    req_desc: &FieldDesc,
    order: ByteOrder,
    decode_cache: &mut TypeCache,
) -> Result<Option<PvField>, String> {
    if cur.position() as usize >= cur.get_ref().len() {
        return Ok(None);
    }
    decode_pv_field_cached(req_desc, cur, order, decode_cache)
        .map(Some)
        .map_err(|e| format!("invalid pvRequest value: {e}"))
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
    // NULL (0xFF) type code: parameterless RPC (pvxs interop). Peek so
    // we can consume exactly the one byte without depending on
    // `decode_type_desc`'s error-vs-consume behaviour for 0xFF.
    if cur.get_ref()[cur.position() as usize] == 0xFF {
        cur.set_position(cur.position() + 1);
        return Ok((FieldDesc::Variant, PvField::Null));
    }
    // Present, non-null descriptor: decode type + full value or fail
    // fatally — a present-but-undecodable body is a protocol error, not
    // a parameterless call.
    let desc = decode_type_desc_cached(cur, order, decode_cache)
        .map_err(|e| format!("invalid RPC argument descriptor: {e}"))?;
    let value = decode_pv_field_cached(&desc, cur, order, decode_cache)
        .map_err(|e| format!("invalid RPC argument value: {e}"))?;
    Ok((desc, value))
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
        monitor_resume: Arc::new(tokio::sync::Notify::new()),
        monitor_wm: None,
        monitor_wm_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        monitor_op_id: next_op_id(),
        monitor_filters: Arc::new(epics_base_rs::server::database::filters::FilterChain::new()),
        put_auto_exec: true,
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
            "PUT_GET not served (serve_put_get disabled for pvxs compatibility)",
            order,
        )
        .await?;
        return Ok(());
    }

    // Connection-wide IOID uniqueness (pvxs `ServerConn::opByIOID`),
    // evaluated before the per-channel borrow below.
    let dup_ioid = subcmd & QosFlags::INIT != 0 && ioid_live_on_conn(channels, ioid);

    let ch = match channels.get_mut(&sid) {
        Some(c) => c,
        None => {
            send_op_error(
                tx,
                OpKind::PutGet,
                ioid,
                subcmd,
                "unknown channel sid",
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
                "max ops per channel exceeded",
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
                    "must provide prototype",
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
        let req_desc = match decode_type_desc_cached(&mut cur, inbound_order, decode_cache) {
            Ok(d) => d,
            Err(e) => {
                return Err(PvaError::Decode(format!(
                    "PUT_GET INIT pvRequest descriptor: {e}"
                )));
            }
        };
        let req_value =
            match decode_init_pv_request_value(&mut cur, &req_desc, inbound_order, decode_cache) {
                Ok(v) => v,
                Err(e) => {
                    return Err(PvaError::Decode(format!(
                        "PUT_GET INIT pvRequest value: {e}"
                    )));
                }
            };
        // empty mask is an INIT error.
        let mask = match crate::pv_request::request_to_mask(&intro, &req_desc) {
            Ok(m) => m,
            Err(e) => {
                send_chan_op_error(
                    &chan_tx,
                    OpKind::PutGet,
                    ioid,
                    subcmd,
                    &format!("invalid pvRequest mask: {e}"),
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
        let mut put_get_op = non_monitor_op_state(intro.clone(), OpKind::PutGet, mask);
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
    let (intro, mask, init_pv_request) = match op {
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
            (o.intro, o.mask, o.pv_request)
        }
        None => {
            send_chan_op_error(
                &chan_tx,
                OpKind::PutGet,
                ioid,
                subcmd,
                "operation not initialised",
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
    // subcommand. Our INIT serves the same introspection for the put and
    // get structures (see above), so both read-only legs reply with the
    // current value under the negotiated mask, echoing the subcommand the
    // client dispatches on (getGetDone vs getPutDone).
    let read_only = subcmd & (QosFlags::GET | QosFlags::GET_PUT) != 0;

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
    let join = tokio::spawn(async move {
        // return this op to `Idle` (via the read-loop owner) when the
        // task ends so a later explicit re-EXEC is accepted.
        let _exec_fin_guard = ExecFinishGuard {
            tx: exec_fin_tx_task,
            fin: exec_fin,
        };
        let mut payload = Vec::new();
        payload.put_u32(ioid, order);
        payload.put_u8(subcmd);

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
        let read_value: Result<Option<PvField>, String> =
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
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(panic) => Err(panic),
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
                catch_handler_panic(src.get_value_checked(read_checked, ctx)).await
            };

        match read_value {
            Ok(Some(v)) => {
                Status::ok().write_into(order, &mut payload);
                let wire_changed = crate::pvdata::encode::canonical_changed_bitset(&intro, &mask);
                wire_changed.write_into(order, &mut payload);
                crate::pvdata::encode::encode_pv_field_with_bitset(
                    &v,
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
            Err(msg) => Status::error(msg).write_into(order, &mut payload),
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
    finish_exec_data_task(ch, ioid, subcmd, join.abort_handle());
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

    let ch = match channels.get_mut(&sid) {
        Some(c) => c,
        None => {
            send_op_error(
                tx,
                OpKind::Process,
                ioid,
                subcmd,
                "unknown channel sid",
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
                "max ops per channel exceeded",
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
                    "must provide prototype",
                    order,
                )
                .await?;
                return Ok(());
            }
        };
        // route the PROCESS INIT pvRequest through the SAME
        // structured boundary as the generic GET/PUT/MONITOR INIT path
        // (`decode_init_pv_request_value`). PROCESS transfers no value,
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
        // An ABSENT value body is still tolerated for Rust↔Rust interop.
        let req_desc = match decode_type_desc_cached(&mut cur, inbound_order, decode_cache) {
            Ok(d) => d,
            Err(e) => {
                return Err(PvaError::Decode(format!(
                    "PROCESS INIT pvRequest descriptor: {e}"
                )));
            }
        };
        let req_value =
            match decode_init_pv_request_value(&mut cur, &req_desc, inbound_order, decode_cache) {
                Ok(v) => v,
                Err(e) => {
                    return Err(PvaError::Decode(format!(
                        "PROCESS INIT pvRequest value: {e}"
                    )));
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
    let join = tokio::spawn(async move {
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
        let result = catch_handler_panic(src.process_checked(checked, ctx))
            .await
            .map_err(|e| OpError::failed(e))
            .and_then(|r| r);

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
        let _ = tx_clone.send(buf).await;
    });
    // Store the abort guard and, when this PROCESS frame carried the
    // last-request bit (`subcmd & 0x10`), defer the op's removal until its
    // reply has been sent — the same completion-owned cleanup GET/PUT/RPC use
    // (see [`finish_exec_data_task`]).
    finish_exec_data_task(ch, ioid, subcmd, join.abort_handle());
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
///   field, call [`ChannelSource::channel_array_init`], and reply
///   `ioid + subcmd + status [+ array introspection]`. The default source
///   refuses with a protocol `Status` error — the client always gets an
///   answer, never the pre-fix silent drop.
/// - getArray (`QOS_GET 0x40`): `offset + count + stride` → READ-gated
///   [`ChannelSource::channel_array_get`] → `status + array value`.
/// - setLength (`QOS_GET_PUT 0x80`): `length` → WRITE-gated
///   [`ChannelSource::channel_array_set_length`] → `status`.
/// - getLength (`QOS_PROCESS 0x04`): no payload → READ-gated
///   [`ChannelSource::channel_array_get_length`] → `status + length`.
/// - putArray (no get/length bits): `offset + stride + array value` →
///   WRITE-gated [`ChannelSource::channel_array_put`] → `status`.
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

    let ch = match channels.get_mut(&sid) {
        Some(c) => c,
        None => {
            send_op_error(
                tx,
                OpKind::Array,
                ioid,
                subcmd,
                "unknown channel sid",
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
                "max ops per channel exceeded",
                order,
            )
            .await?;
            return Ok(());
        }
        // Decode the pvRequest selecting the array field, through the same
        // structured boundary as GET/PUT/PROCESS INIT. A present-but-
        // malformed descriptor or value is a connection-fatal peer wire
        // fault (pvxs `from_wire_type_value`); an absent body is tolerated.
        let req_desc = match decode_type_desc_cached(&mut cur, inbound_order, decode_cache) {
            Ok(d) => d,
            Err(e) => {
                return Err(PvaError::Decode(format!(
                    "ARRAY INIT pvRequest descriptor: {e}"
                )));
            }
        };
        let req_value =
            match decode_init_pv_request_value(&mut cur, &req_desc, inbound_order, decode_cache) {
                Ok(v) => v,
                Err(e) => {
                    return Err(PvaError::Decode(format!("ARRAY INIT pvRequest value: {e}")));
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
        };
        // INIT resolves the array field's introspection (or refuses). No
        // access check here — pvAccessCPP gates per sub-op, not at create.
        match source.channel_array_init(&pv_name, ctx).await {
            Ok(array_desc) => {
                let mask = BitSet::all_set(array_desc.total_bits());
                let mut array_op = non_monitor_op_state(array_desc.clone(), OpKind::Array, mask);
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
                send_chan_op_error(&chan_tx, OpKind::Array, ioid, subcmd, &e.message, order)
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
                "bad request id",
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
                "other request pending",
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
    let join = tokio::spawn(async move {
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
            Err(e) => Status::error(e.message).write_into(order, &mut payload),
        }
        let h = PvaHeader::application(true, order, Command::Array.code(), payload.len() as u32);
        let mut buf = Vec::new();
        h.write_into(&mut buf);
        buf.extend_from_slice(&payload);
        let _ = tx_clone.send(buf).await;
    });
    finish_exec_data_task(ch, ioid, subcmd, join.abort_handle());
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
        // Peek the header length once we have 8 bytes — if an opt-in
        // `max_msg_size` is set and the peer claimed more, drop the
        // connection before growing rx_buf any further.
        // `None` = unbounded (pvxs parity, which keeps no RX cap).
        // Even unbounded, the read stays incremental (4 KiB chunks)
        // and `op_timeout`-deadlined, so a stalled or oversized peer
        // is bounded by the deadline rather than the cap.
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
        let conn_ctx = crate::server_native::source::ChannelContext {
            peer,
            account: cred.account.clone(),
            method: cred.method.clone(),
            host: cred.host.clone(),
            authority: cred.authority.clone(),
            roles: cred.roles.clone(),
            pv_request: None,
        };
        tokio::spawn(async move {
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
                    let intro = owner.get_introspection_checked(&nm, conn_ctx.clone()).await;
                    Some(ResolvedChannel { intro, owner })
                } else {
                    None
                };
                let _ = cc
                    .send(CreateChannelCompletion {
                        cid,
                        sid,
                        name: nm,
                        resolved,
                    })
                    .await;
            }
        });
    }
    Ok(())
}

/// Build the connection-scoped [`ChannelContext`] for a channel
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
    }
}

/// Tear a single channel down and deliver its `onClose` to the bound
/// source, in pvxs `ServerChan::cleanup` order: drop the per-op state
/// first — aborting any monitor subscriber tasks and releasing
/// source-side subscriptions — then notify the source exactly once
/// (`serverchan.cpp:43-60`, `:115-127`). `peer`/`cred` reconstruct the
/// connection-scoped identity the channel was opened under.
fn close_channel(ch: ChannelState, peer: SocketAddr, cred: &ClientCredentials) {
    let ChannelState {
        name, source, ops, ..
    } = ch;
    drop(ops);
    let ctx = channel_lifecycle_ctx(peer, cred);
    source.notify_channel_close(&name, &ctx);
}

/// Connection-scoped context for a channel teardown that emits a
/// DESTROY_CHANNEL frame: the writer handle, the outbound byte order, the
/// peer's address + credentials (to reconstruct the channel's lifecycle
/// identity for the source `onClose`), and the per-peer report registry.
/// Bundling these five keeps the teardown helpers
/// ([`finalize_channel_destroy`], [`handle_destroy_channel`],
/// [`invalidate_named_channels`]) within the argument-count budget and lets
/// the read loop build the context once per use (built per-use rather than
/// once per connection because the connection's `cred` is reassigned during
/// the validation handshake, so a borrow cannot outlive a single arm).
struct ChannelTeardownCtx<'a> {
    tx: &'a SrvTx,
    order: ByteOrder,
    peer: SocketAddr,
    cred: &'a ClientCredentials,
    peer_entry: &'a Arc<crate::server_native::peers::PeerEntry>,
}

/// Why a channel is being torn down with a DESTROY_CHANNEL frame. pvxs
/// attributes the 16-byte reply to the report differently for the two
/// causes, so the single teardown owner must know which one it serves
/// (Regression R0604-PVASRV-REPORT-CHANNEL-BYTE-ACCOUNTING-1).
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
    close_channel(ch, ctx.peer, ctx.cred);

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

    let Some(req) = super::udp::parse_search_request(&raw) else {
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

    // Default protocol on TCP is "tcp" (or "tls" when TLS is in use).
    // Protocol-gated match set, shared with the v4/v6 UDP responders.
    // See `udp::search_matched_cids`.
    let protocol: &'static str = if config.tls.is_some() { "tls" } else { "tcp" };
    // pvxs fills `Search::source` from the TCP peer for circuit search
    // (serverchan.cpp:197-222), so a source can scope advertisement by
    // the established peer endpoint.
    let matched = super::udp::search_matched_cids(source, &req, protocol, peer).await;
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
    frame: &Frame,
    tx: &SrvTx,
    channels: &mut HashMap<u32, ChannelState>,
    order: ByteOrder,
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
                "max ops per channel exceeded",
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
            (OpKind::Rpc, None) => FieldDesc::Variant,
            (_, Some(d)) => d,
            (_, None) => {
                send_chan_op_error(
                    &chan_tx,
                    kind,
                    ioid,
                    subcmd,
                    "must provide prototype",
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
        // below is different — pvxs builds the mask inside the source's
        // `onOp` (`serverget.cpp:198-203`), whose throw is caught and
        // signalled as a remote *op* error (`serverget.cpp:407-413`),
        // so that stays an op-level Status reply.
        let req_desc = match decode_type_desc_cached(&mut cur, inbound_order, decode_cache) {
            Ok(d) => d,
            Err(e) => {
                return Err(PvaError::Decode(format!("INIT pvRequest descriptor: {e}")));
            }
        };
        // VALUE body: an ABSENT body is tolerated (the Rust client's RPC
        // INIT sends only the descriptor — interop), but a PRESENT but
        // malformed one is the same `!M.good()` bev.reset() wire fault as
        // the descriptor above, so it is likewise connection-fatal.
        let req_value =
            match decode_init_pv_request_value(&mut cur, &req_desc, inbound_order, decode_cache) {
                Ok(v) => v,
                Err(e) => {
                    return Err(PvaError::Decode(format!("INIT pvRequest value: {e}")));
                }
            };
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
                send_chan_op_error(
                    &chan_tx,
                    kind,
                    ioid,
                    subcmd,
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
        let pipeline_req = req_value.as_ref().and_then(monitor_pipeline_options);
        // The client-requested `queueSize` overrides the server's default
        // monitor queue depth for THIS operation, whether or not pipeline
        // flow control is enabled (pvxs `op->limit = qSize` sits outside
        // `if(op->pipeline)`, servermon.cpp:533-543). Captured before
        // `pipeline_req` is consumed by the move below; `None` means the
        // server keeps its configured `monitor_queue_depth`.
        let requested_queue_size = match &pipeline_req {
            Some(MonitorPipelineRequest::Options(o)) => o.requested_queue_size,
            _ => None,
        };
        // pvxs `servermon.cpp:537-540`: a MONITOR pipeline request whose
        // PRESENT `queueSize` is invalid (`<2`/unparseable) is a
        // negotiation error — reject the INIT (`ctrl->error(...)` +
        // `return`) instead of silently downgrading to a non-pipeline
        // monitor. GET/PUT/RPC never negotiate pipeline (pvxs
        // `serverget` ignores these options), so the reject is
        // monitor-only.
        if kind == OpKind::Monitor && matches!(pipeline_req, Some(MonitorPipelineRequest::Reject)) {
            send_chan_op_error(
                &chan_tx,
                kind,
                ioid,
                subcmd,
                "can not pipeline invalid queueSize (must be >= 2)",
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
        // below consumes `pipeline_req`. Regression
        // R0604-PVASRV-MONITOR-OPTION-REMOTE-LOG-1.
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
                                &format!("invalid channel filter: {e}"),
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
                queue_size: requested_queue_size,
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
                monitor_resume: Arc::new(tokio::sync::Notify::new()),
                monitor_wm,
                monitor_wm_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
                monitor_op_id: next_op_id(),
                monitor_filters,
                put_auto_exec,
                pv_request: stashed_pv_request,
                monitor_options,
                data_task_abort: None,
                monitor_start_ctl: None,
                exec_state: ExecState::Idle,
                last_request: false,
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
            let join = tokio::spawn(async move {
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
                let value = match catch_handler_panic(src.get_value_checked(checked, ctx)).await {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        let _ = send_chan_op_error(
                            &tx_clone,
                            OpKind::Get,
                            ioid,
                            subcmd,
                            "PV not found",
                            order,
                        )
                        .await;
                        return;
                    }
                    Err(msg) => {
                        let _ =
                            send_chan_op_error(&tx_clone, OpKind::Get, ioid, subcmd, &msg, order)
                                .await;
                        return;
                    }
                };
                // source-side mismatch gate.
                if let Err(e) = crate::pvdata::value_matches_descriptor(&value, &intro_t) {
                    let _ = send_chan_op_error(
                        &tx_clone,
                        OpKind::Get,
                        ioid,
                        subcmd,
                        &format!("source value does not match opened descriptor: {e}"),
                        order,
                    )
                    .await;
                    return;
                }
                let mut payload = Vec::new();
                payload.put_u32(ioid, order);
                payload.put_u8(subcmd);
                Status::ok().write_into(order, &mut payload);
                let changed = crate::pvdata::encode::canonical_changed_bitset(&intro_t, &mask_t);
                changed.write_into(order, &mut payload);
                crate::pvdata::encode::encode_pv_field_with_bitset(
                    &value,
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
            finish_exec_data_task(ch, ioid, subcmd, join.abort_handle());
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
                let join = tokio::spawn(async move {
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
                    let value = match catch_handler_panic(src.get_value_checked(checked, ctx)).await
                    {
                        Ok(Some(v)) => v,
                        Ok(None) => {
                            let _ = send_chan_op_error(
                                &tx_clone,
                                OpKind::Put,
                                ioid,
                                subcmd,
                                "PV not found",
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
                                &msg,
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
                    let changed =
                        crate::pvdata::encode::canonical_changed_bitset(&intro_t, &mask_t);
                    changed.write_into(order, &mut payload);
                    crate::pvdata::encode::encode_pv_field_with_bitset(
                        &value,
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
                finish_exec_data_task(ch, ioid, subcmd, join.abort_handle());
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
            let join = tokio::spawn(async move {
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
                };
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
                            match catch_handler_panic(src.get_value_checked(read_checked, ctx))
                                .await
                            {
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
                    Err(msg) => {
                        Status::error(msg).write_into(order, &mut payload);
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
            finish_exec_data_task(ch, ioid, subcmd, join.abort_handle());
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
            // Regression R0604-PVASRV-MONITOR-ACK-VALIDATE-ORDER-1.
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
            // after the ACK block). The ACK count for a combined frame was
            // already validated at the top of this arm, so reaching here means
            // the frame decoded cleanly and these side effects are safe.
            // Regression R0604-PVASRV-MONITOR-ACK-VALIDATE-ORDER-1.
            if let Some(op) = ch.ops.get(&ioid) {
                if is_stop {
                    op.monitor_paused
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    // Executing->Idle. The op's single
                    // start-control owner fires notify_monitor_start(false)
                    // on the edge so a gateway can suspend its upstream.
                    if let Some(ctl) = &op.monitor_start_ctl {
                        ctl.set(false);
                    }
                } else if is_start {
                    let prev = op
                        .monitor_paused
                        .swap(false, std::sync::atomic::Ordering::Relaxed);
                    if prev {
                        if let Some(n) = op.monitor_window_notify.as_ref() {
                            n.notify_waiters();
                        }
                        // wake the subscriber loop so it flushes
                        // the value squashed during the pause (works for
                        // non-pipelined ops too, which have no window
                        // notify). `notify_one` stores a permit when the
                        // loop isn't waiting yet, so a resume that races
                        // ahead of the loop's `notified()` is not lost.
                        op.monitor_resume.notify_one();
                        // Idle->Executing. `prev` gates this to a
                        // genuine resume — a START on a monitor that was not
                        // actually paused does not re-fire on_start(true).
                        if let Some(ctl) = &op.monitor_start_ctl {
                            ctl.set(true);
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
            if is_start && !already_running {
                let pv_name = ch.name.clone();
                let intro_clone = intro.clone();
                let mask_clone = mask.clone();
                let tx_clone = chan_tx.clone();
                let src = source.clone();
                // Per-operation monitor queue depth: the client-requested
                // `record._options.queueSize` (captured at INIT into
                // `monitor_options.queue_size`) overrides the server-wide
                // default, matching pvxs `op->limit = qSize` for both
                // pipeline and plain monitors (servermon.cpp:533-543). The
                // squash threshold below keys on this, not the config
                // default, so `record[queueSize=2]` squashes at 2.
                let queue_depth = ch
                    .ops
                    .get(&ioid)
                    .and_then(|s| s.monitor_options.queue_size)
                    .map(|n| n as usize)
                    .unwrap_or(config.monitor_queue_depth);
                let high_watermark = config.monitor_high_watermark;
                // ACF-aware MONITOR: capture the peer's credentials
                // so the spawned task can consult ctx-aware
                // subscribe/get_value paths. Sources without ACF
                // delegate to the legacy methods.
                // forward the INIT pvRequest so the source can
                // honor `record._options.DBE` (per-op database event-
                // mask selection — pvxs singlesource.cpp:115). As
                // with PUT, the data-phase START/ACK frames are
                // pure stream control; per-operation options live in
                // the INIT pvRequest only.
                let mon_ctx = crate::server_native::source::ChannelContext {
                    peer,
                    account: cred.account.clone(),
                    method: cred.method.clone(),
                    host: cred.host.clone(),
                    authority: cred.authority.clone(),
                    roles: cred.roles.clone(),
                    pv_request: init_pv_request.clone(),
                };
                // Type-state MONITOR gate.
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
                // Snapshot the window + notify so the spawned task can
                // share state with this dispatch path's ACK handler.
                // also lift the event-affecting monitor
                // options so they reach the source's
                // `subscribe_*_checked_opts`.
                let (
                    window,
                    window_notify,
                    paused_flag,
                    resume_notify,
                    filters,
                    monitor_options,
                    wm_seq,
                    monitor_op_id,
                    wm_levels_init,
                ) = ch
                    .ops
                    .get(&ioid)
                    .map(|s| {
                        (
                            s.monitor_window.clone(),
                            s.monitor_window_notify.clone(),
                            s.monitor_paused.clone(),
                            s.monitor_resume.clone(),
                            s.monitor_filters.clone(),
                            s.monitor_options.clone(),
                            // the LOW callback in the loop
                            // below crosses this shared counter back to
                            // "below"; the HIGH callback in the ACK dispatch
                            // crosses it to "above". Sharing it keeps the
                            // pause/resume hysteresis AND the monotonic
                            // ordering token coherent across the two paths.
                            s.monitor_wm_seq.clone(),
                            // same op identity the ACK
                            // HIGH uses, so both votes (and the teardown
                            // Withdraw) reference-count under one key.
                            s.monitor_op_id,
                            // pvxs ackAt parity: the INIT-clamped watermark
                            // levels (see [`clamp_watermarks`]). The LOW
                            // crossing below reads THIS, not a fresh source
                            // read, so it shares the HIGH path's clamped
                            // levels and honors `ackAny` identically.
                            s.monitor_wm,
                        )
                    })
                    .unwrap_or_else(|| {
                        (
                            None,
                            None,
                            Arc::new(std::sync::atomic::AtomicBool::new(false)),
                            Arc::new(tokio::sync::Notify::new()),
                            Arc::new(epics_base_rs::server::database::filters::FilterChain::new()),
                            crate::server_native::source::MonitorOptions::default(),
                            Arc::new(std::sync::atomic::AtomicU64::new(1)),
                            next_op_id(),
                            None,
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
                // capture this op instance's terminal-signal payload
                // (sid + ioid + the process-unique `monitor_op_id`) before
                // the move. The guard installed as the first statement of the
                // task body reports it on EVERY exit so the read-loop owner
                // removes the op — the task itself owns no `ch.ops` handle.
                let mon_fin = MonitorFinished {
                    sid,
                    ioid,
                    op_id: monitor_op_id,
                };
                let mon_fin_tx_task = mon_fin_tx.clone();
                // Per-op executing-state channel for the monitor START/STOP
                // gate. `MonitorStartControl` (read-loop side) publishes each
                // Executing<->Idle edge here; the spawned task drives the
                // source-supplied `SubscriptionSeed::on_start` gate from it.
                // Starts `false` (Idle) — the first MONITOR START fires the
                // edge to `true`. Created before the spawn so the receiver is
                // captured by the task and the sender reaches the op's
                // `MonitorStartControl` below.
                let (monitor_exec_tx, monitor_exec_rx) = tokio::sync::watch::channel(false);
                let join = tokio::spawn(async move {
                    // terminal finalizer — see `MonitorFinishGuard`.
                    // A single local at the top of the body so no exit (source
                    // close FINISH, ACL/descriptor/filter/raw terminal return,
                    // panic, abort) can skip notifying the owner.
                    let _fin_guard = MonitorFinishGuard {
                        tx: mon_fin_tx_task,
                        fin: mon_fin,
                    };
                    // access gate check moved inside the spawn
                    // so the read loop is not blocked while the ACF
                    // policy resolves. The version capture must precede
                    // the check (see audit note above).
                    let mon_acl_version_at_subscribe_cell = Arc::new(
                        std::sync::atomic::AtomicU64::new(src.access_gate().acl_version()),
                    );
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
                    // raw-frame fast path. When the source can
                    // hand us pre-encoded MONITOR DATA bytes (e.g.
                    // pva_gateway upstream-monitor task already
                    // received them on the wire), emit them with only
                    // an IOID-rewrite — pvxs / pva2pva style raw
                    // forward. Falls back to the decoded path on
                    // byte-order mismatch or when the source returns
                    // None.
                    // Raw fast path must consult
                    // the ACF too. The earlier ACL gate covered
                    // `subscribe_ctx` only; ACF-aware sources can now
                    // override `subscribe_raw_ctx` to deny when the
                    // peer lacks READ. When the gateway denies (returns
                    // None), we fall through to the decoded
                    // `subscribe_ctx` below — which is also ACF-gated
                    // and will likewise return None.
                    if raw_path_eligible
                        && let Some(seed_raw) = src
                            .subscribe_raw_seeded(
                                mon_checked.clone(),
                                mon_ctx.clone(),
                                monitor_options.clone(),
                            )
                            .await
                    {
                        // Single MONITOR seed owner (raw path): the seed
                        // is the source's cached snapshot, captured with
                        // the subscription; the server no longer issues
                        // its own `get_value` seed (which, for the
                        // gateway, was a fresh upstream GET that diverged
                        // from the pvxs cached-`lastelem` seed).
                        let crate::server_native::source::SubscriptionSeed {
                            initial: seed_raw_initial,
                            updates: mut rx_raw,
                            // raw fast path is the fanout gateway, which gates
                            // its shared upstream per `(name, ctx)` via
                            // `notify_monitor_start`, not per op — no per-op gate.
                            on_start: _,
                        } = seed_raw;
                        // Revalidate ACL BEFORE sending the
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
                            // Route the re-check
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
                        // Emit the single connect-time seed via the
                        // regular encode path (no raw bytes for the
                        // first-event seed; the cache may not have them
                        // yet). `seed_raw_initial` is the source's cached
                        // snapshot; `None` means no value yet or NoAccess
                        // on this PV's ASG — no initial frame either way.
                        if let Some(initial) = seed_raw_initial {
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
                        // raw fast path honors the pause gate
                        // through the same owner as the decoded path.
                        // `held_raw` carries the latest squashed event
                        // across a pause; `type_changed` is a boundary
                        // that the owner yields immediately even while
                        // paused (it tears the stream down — it must not
                        // be held behind a later descriptor-incompatible
                        // event).
                        let mut held_raw: Option<crate::server_native::RawMonitorEvent> = None;
                        while let Some(ev) = next_monitor_event(
                            &mut rx_raw,
                            &mut held_raw,
                            &paused_flag,
                            &resume_notify,
                            |ev| ev.type_changed,
                            // Raw events carry no marked set; a pause-time
                            // collapse keeps the newer pre-encoded body.
                            |_, new| new,
                        )
                        .await
                        {
                            // an upstream descriptor change
                            // arrives as a `type_changed=true` marker
                            // event. The body bytes are encoded for
                            // the NEW upstream descriptor but this
                            // monitor was negotiated against the OLD
                            // (now-stale) `intro_clone` at INIT.
                            // Forwarding the body would deliver
                            // garbage / cause a client-side protocol
                            // error (pvxs treats this as a
                            // subscription boundary —
                            // pvalink_channel.cpp:342-351). Emit
                            // MONITOR FINISH so the client knows to
                            // reopen against the new descriptor, and
                            // tear down this monitor task.
                            if ev.type_changed {
                                let finish = build_monitor_finish(ioid, order);
                                let _ = tx_clone.send(finish).await;
                                return;
                            }
                            // ACL re-check on
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
                            // a pause that began after the
                            // owner handed back this event (during the
                            // ACL revalidation await above) must HOLD it
                            // — squash to latest and resume-flush —
                            // exactly as the decoded path does once a
                            // value is already in hand. Producing no
                            // wire frame consumes no pipeline credit.
                            if paused_flag.load(std::sync::atomic::Ordering::Relaxed) {
                                held_raw = Some(ev);
                                continue;
                            }
                            // on byte-order mismatch we decode the
                            // raw event under the upstream order and
                            // re-encode under the downstream order. Earlier
                            // code dropped the event with `continue`, so any
                            // cross-host gateway between peers with different
                            // negotiated byte orders silently lost every
                            // monitor update after the initial snapshot (the
                            // decoded-fallback path never sees those events
                            // under raw subscription). a re-encode
                            // failure (malformed upstream body) is now a
                            // terminal protocol boundary, not a silent drop —
                            // `raw_monitor_frame` owns that single policy so
                            // the same-endian forward and the cross-endian
                            // re-encode cannot diverge on malformed input.
                            let payload = match raw_monitor_frame(ioid, &intro_clone, &ev, order) {
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
                        let finish = build_monitor_finish(ioid, order);
                        let _ = tx_clone.send(finish).await;
                        return;
                    }

                    // Single MONITOR seed owner: the source returns the
                    // connect-time seed AND the post-seed update stream
                    // as one `SubscriptionSeed`, so the server emits the
                    // seed below from `seed_initial` and never issues its
                    // own `get_value` seed — closing the double-seed
                    // (server get_value + a self-seeding source stream).
                    let Some(seed) = src
                        .subscribe_seeded(
                            mon_checked.clone(),
                            mon_ctx.clone(),
                            monitor_options.clone(),
                        )
                        .await
                    else {
                        return;
                    };
                    let crate::server_native::source::SubscriptionSeed {
                        initial: seed_initial,
                        updates: mut rx,
                        on_start: seed_on_start,
                    } = seed;
                    // Per-op MONITOR START/STOP gate. When the source backs
                    // this op with suspendable upstream subscriptions (QSRV
                    // DbSubscriptions), drive its enable/disable from this op's
                    // Executing<->Idle edge — published on `monitor_exec_rx` by
                    // `MonitorStartControl` (read-loop side). The driver lives
                    // in its own task so the hot update loop below is untouched;
                    // it ends when the op tears down and drops `monitor_exec_tx`
                    // (the backing subscriptions are then removed with the
                    // aborted monitor, so STOP=disable / teardown=remove).
                    if let Some(gate) = seed_on_start {
                        spawn_monitor_gate_driver(gate, monitor_exec_rx);
                    }
                    // Diagnostic-only outbound-queue-depth crossing flag.
                    let mut queue_over_high = false;
                    // per-PV pipeline-window watermark levels
                    // `(low, high)` in credit units. `None` when the source
                    // exposes no per-PV levels. the hysteresis
                    // state + ordering token is the SHARED `wm_seq` counter
                    // (crossed to "above" by the ACK dispatch, back to
                    // "below" here) so a gateway upstream paused on LOW can
                    // be resumed by the credit-refill HIGH callback. These
                    // are the INIT-clamped levels threaded out of `OpState`
                    // (pvxs ackAt parity, [`clamp_watermarks`]) — the same
                    // value the HIGH path reads via `op.monitor_wm`, NOT a
                    // fresh `monitor_watermarks` call, so `ackAny` clamps
                    // both crossings through one owner.
                    let wm_levels = wm_levels_init;
                    // Single owner of this monitor's pipeline-credit
                    // accounting, shared by the initial-snapshot send and
                    // the update loop so the initial frame consumes a
                    // window slot exactly like every subsequent DATA
                    // frame (pvxs `servermon.cpp:192`). Holds shared
                    // borrows for the rest of the task body.
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
                    // arm the withdraw-on-teardown
                    // finalizer for flow-controlled (gateway) ops only.
                    // Held live for the rest of the task so every exit
                    // path — normal end, early `return`, or AbortOnDrop
                    // cancelling this task on DESTROY/disconnect — drops
                    // it and withdraws this op's upstream pause vote (see
                    // [`WatermarkWithdrawOnDrop`]). `pv_request` is
                    // irrelevant to upstream-cache selection, so the ctx
                    // mirrors the HIGH path and omits it.
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
                    // Revalidate ACL BEFORE
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
                    // does this source emit *partial* monitor
                    // updates (QSRV group monitor with a self-trigger)?
                    // When it does, every event after the first carries
                    // a wire changed-bitset narrowed to the leaves that
                    // actually changed — derived by structurally
                    // diffing consecutive snapshots, matching pvxs's
                    // marked-leaf semantics (`servermon.cpp:174`
                    // `to_wire_valid(R, ent, &pvMask)`). `prev_value`
                    // holds the last emitted snapshot for that diff.
                    let emits_partial = src.monitor_emits_partial(&pv_name).await;
                    let mut prev_value: Option<PvField> = None;
                    // Emit the single connect-time seed carried back by
                    // `subscribe_seeded`. `None` means the source has no
                    // current value yet (unopened SharedPV / gateway entry
                    // awaiting its first upstream event) or a peer with
                    // NoAccess on the record's ASG — emit nothing and let
                    // the update loop deliver the first real event.
                    if let Some(initial) = seed_initial {
                        // Finding #2: run the server `_filter` chain on the
                        // FIRST frame too, through the same owner the update
                        // loop uses — epics-base `dbChannelRunPreChain`
                        // filters every monitor event, so `arr` must slice
                        // the initial snapshot, not just updates. A gating
                        // filter (`dbnd`/`dec`/`sync`) that drops the first
                        // event suppresses the initial frame; the update
                        // loop then delivers the first passing one.
                        let initial =
                            match apply_monitor_filter_chain(&filters, &initial, &intro_clone) {
                                MonitorFilterOutcome::Pass => Some(initial),
                                MonitorFilterOutcome::Transformed(tv) => Some(tv),
                                MonitorFilterOutcome::Drop => None,
                                MonitorFilterOutcome::DescriptorMismatch => {
                                    let err = build_monitor_error(
                                        ioid,
                                        "server-side filter transform does not \
                                     fit the monitor descriptor",
                                        order,
                                    );
                                    let _ = tx_clone.send(err).await;
                                    return;
                                }
                            };
                        if let Some(initial) = initial {
                            // pvxs `servermon.cpp:261` always lets
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
                            // `mask_clone` on the wire. The first event is
                            // always full (pvxs builds a fresh fully-marked
                            // Value); partial narrowing starts at event #2.
                            let payload = build_monitor_payload(
                                ioid,
                                &intro_clone,
                                &initial,
                                &mask_clone,
                                order,
                            );
                            if emits_partial {
                                prev_value = Some(initial);
                            }
                            // pvxs `servermon.cpp:192` decrements `window`
                            // for the initial snapshot too — consume one
                            // credit through the single owner before
                            // sending, or the client's window drifts to
                            // queueSize + 1.
                            credit.acquire().await;
                            if tx_clone.send(payload).await.is_err() {
                                return;
                            }
                        }
                    }
                    // Server-side monitor queue (pvxs servermon.cpp:271-287,
                    // drained one-per-reply at :171-183). A short burst below
                    // the negotiated depth is delivered as distinct DATA
                    // frames; coalescing happens only as the overflow action
                    // once the queue is full. See `drain_monitor_queue`.
                    let queue_limit = queue_depth.max(1);
                    let mut pending: std::collections::VecDeque<
                        crate::server_native::MonitorUpdate,
                    > = std::collections::VecDeque::new();
                    // value squashed while paused, flushed on
                    // resume. `None` between batches.
                    let mut held: Option<crate::server_native::MonitorUpdate> = None;
                    loop {
                        // Acquire the next value to consider emitting
                        // through the shared pause owner: a value held
                        // from a pause takes priority once resumed; while
                        // paused we keep receiving and squash to the
                        // latest without emitting, waking on resume to
                        // flush it (pvxs queues posts while Idle and
                        // drains on START). A `type_changed` boundary is
                        // the one exception: `|u| u.type_changed` yields
                        // it immediately even while paused, so an upstream
                        // descriptor change is never squashed behind a
                        // later value. Events coalesced during a pause
                        // union their marked-leaf sets via
                        // `coalesce_monitor_update`.
                        // Emit a queued entry first — one per reply (pvxs
                        // servermon.cpp:171-183) — and only block for a fresh
                        // event when the queue is empty. While paused we must
                        // NOT drain the queue: fall through to the pause-aware
                        // `next_monitor_event`, which holds the latest, and
                        // the pause branch below folds the queue into `held`.
                        let mut value = if !paused_flag.load(std::sync::atomic::Ordering::Relaxed)
                            && let Some(front) = pending.pop_front()
                        {
                            front
                        } else {
                            match next_monitor_event(
                                &mut rx,
                                &mut held,
                                &paused_flag,
                                &resume_notify,
                                // An upstream descriptor change arrives as a
                                // `type_changed` boundary on the decoded stream
                                // too (the PVA gateway fanout). Yield it
                                // immediately — even while paused — so it is
                                // never squashed behind a later,
                                // descriptor-incompatible value.
                                |u| u.type_changed,
                                coalesce_monitor_update,
                            )
                            .await
                            {
                                Some(v) => v,
                                None => break,
                            }
                        };
                        // Drain immediately-available extras, folding the
                        // in-hand `value` into the SAME bounded queue so a
                        // single owner counts EVERY unsent post against
                        // `queue_limit` — including the front being prepared
                        // for this reply. pvxs counts `queue.front()` against
                        // queueSize (servermon.cpp:271-287) and pops it only in
                        // `doReply()` when a frame is produced
                        // (servermon.cpp:171-183); holding `value` OUTSIDE the
                        // queue (as before) let `queue_limit + 1` distinct
                        // posts go unsent under backpressure. The drain returns
                        // the (possibly squashed) front as the next `value`.
                        let (next_value, overflow) =
                            drain_monitor_queue(&mut rx, value, &mut pending, queue_limit);
                        value = next_value;
                        if overflow {
                            debug!(
                                pv = %pv_name,
                                queue_limit,
                                "monitor queue full — squashed newest into tail"
                            );
                        }
                        // Subscription boundary: the upstream descriptor
                        // changed. The decoded values that follow are shaped
                        // for the NEW descriptor and would be mis-encoded
                        // against the `intro_clone` negotiated at this
                        // monitor's INIT. Emit MONITOR FINISH so the client
                        // reopens with a fresh INIT — the decoded-path
                        // counterpart of the raw fast path's `type_changed`
                        // branch. This check runs AFTER the drain: at
                        // `queueSize == 1` the front IS the tail, so the drain
                        // can coalesce a boundary post directly into `value`,
                        // and a coalesced boundary MUST be detected on the
                        // finalized `value`, not the pre-drain one. It still
                        // covers both acquisition paths — a boundary popped
                        // from `pending` and one yielded live by
                        // `next_monitor_event` (incl. one surfaced through a
                        // pause-hold coalesce) — because `coalesce_monitor_update`
                        // keeps a boundary sticky and it always reaches the
                        // front before any value it shadowed.
                        if value.type_changed {
                            let finish = build_monitor_finish(ioid, order);
                            let _ = tx_clone.send(finish).await;
                            return;
                        }
                        // ACL re-check on policy reload (same
                        // shape as the raw-fast-path branch above).
                        // The gate's `acl_version` bumps on every
                        // PvaServer ACF swap; on mismatch we
                        // re-mint AccessChecked and tear down with
                        // a MONITOR FINISH if the new policy denies.
                        // Decoded recv-loop
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
                        // outbound-queue depth is a SERVER
                        // diagnostic only — it is no longer used to fire
                        // the SharedPV watermark callbacks. pvxs ties
                        // `onHighMark`/`onLowMark` to the pipeline flow-
                        // control window, which we now do at the credit
                        // gate below. Counter is max_capacity - capacity
                        // since mpsc doesn't expose len directly.
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
                        // pause and filter suppression MUST run
                        // before pipeline credit is consumed. Pipeline
                        // credit accounts for monitor DATA frames sent
                        // to the client (pvxs `servermon.cpp:192`
                        // decrements `window` only after the frame is
                        // enqueued). An event dropped by pause or by
                        // the filter chain produces no wire frame, so
                        // it must not consume a window slot — otherwise
                        // a client with a finite pipeline window stalls
                        // waiting to ACK frames it never received.
                        //
                        // a pause that began while this value was
                        // already in hand must HOLD it (squash to latest),
                        // not drop it — resume flushes the held value. This
                        // mirrors pvxs keeping posts in the monitor queue
                        // while Idle and draining on START. Like the drop
                        // it once was, holding consumes no pipeline credit
                        // (no wire frame is produced).
                        if paused_flag.load(std::sync::atomic::Ordering::Relaxed) {
                            // Paused: emit nothing. Squash the in-hand value
                            // and any queued backlog (front→back, oldest→
                            // newest) into the single held latest — resume
                            // flushes it. The queue models the non-paused
                            // burst; pause keeps the existing hold-latest
                            // semantics and must not leave entries behind.
                            while let Some(q) = pending.pop_front() {
                                value = coalesce_monitor_update(value, q);
                            }
                            held = Some(value);
                            continue;
                        }
                        // past the pause gate the wire frame is
                        // built from the PvField snapshot; the explicit
                        // marked-leaf set (when the source carries one) drives
                        // the changed-bitset below. `take()` leaves the moved
                        // MonitorUpdate inert before we shadow `value`.
                        let marked = value.marked.take();
                        let value = value.value;
                        // Server-side channel filters: skip when the
                        // chain drops this event. Empty chain (the
                        // default) is a no-op pass-through.
                        //
                        // a filter chain may TRANSFORM the
                        // event (e.g. `arr` slices the array, `ts`
                        // rewrites the value). The transformed value
                        // from `FilterChain::apply` is bridged back
                        // into the wire `PvField` via
                        // `apply_filter_transform`; the monitor frame
                        // is then built from the transformed value,
                        // not the original. A pass/drop-only filter
                        // (`dec`, `sync`, scalar `dbnd`) leaves the
                        // value unchanged, so the bridge is a no-op
                        // for it. When the transformed value cannot be
                        // represented in the negotiated monitor
                        // descriptor (a transformation filter whose
                        // output type/shape does not fit this PV's
                        // fixed wire descriptor — e.g. a `ts` mode
                        // that changes the scalar type), the
                        // subscription cannot honor the filter: emit a
                        // monitor error frame and end the stream
                        // rather than silently sending a wrong value.
                        let value = match apply_monitor_filter_chain(&filters, &value, &intro_clone)
                        {
                            MonitorFilterOutcome::Pass => value,
                            MonitorFilterOutcome::Drop => continue,
                            MonitorFilterOutcome::Transformed(tv) => tv,
                            MonitorFilterOutcome::DescriptorMismatch => {
                                let err = build_monitor_error(
                                    ioid,
                                    "server-side filter transform does not \
                                     fit the monitor descriptor",
                                    order,
                                );
                                let _ = tx_clone.send(err).await;
                                return;
                            }
                        };
                        // pipeline window check, through the same
                        // single owner the initial snapshot uses. When
                        // pipeline is active this waits for a free window
                        // slot, consumes one credit, and fires LOW on the
                        // above→below crossing; for a non-pipeline monitor
                        // it is a no-op (mpsc backpressure stays the only
                        // gate). It runs after the pause/filter gates above
                        // so credit is consumed only for events that will
                        // produce a DATA frame.
                        credit.acquire().await;
                        // an explicit marked-leaf set from the
                        // source (a QSRV group `+trigger` target graph) takes
                        // precedence over both server-derived bitsets — pvxs
                        // `groupsource.cpp:288` marks each trigger target
                        // assigned-not-changed, so a value-diff would wrongly
                        // drop targets whose value did not move. The encoder
                        // turns the declared paths into the wire bitset
                        // intersected with the request mask.
                        //
                        // otherwise, for a partial-emitting source,
                        // narrow the wire changed-bitset to exactly the leaves
                        // that differ from the previously emitted snapshot,
                        // intersected with the request mask — pvxs
                        // `to_wire_valid(R, ent, &pvMask)`. The first event
                        // already went out above with the full mask; from here
                        // on `prev_value` is set.
                        let payload = if let Some(paths) = marked.as_ref() {
                            build_monitor_payload_marked(
                                ioid,
                                &intro_clone,
                                &value,
                                paths,
                                &mask_clone,
                                order,
                            )
                        } else if let Some(prev) = prev_value.as_ref() {
                            build_monitor_payload_partial(
                                ioid,
                                &intro_clone,
                                &value,
                                prev,
                                &mask_clone,
                                order,
                            )
                        } else {
                            build_monitor_payload(ioid, &intro_clone, &value, &mask_clone, order)
                        };
                        if emits_partial {
                            prev_value = Some(value.clone());
                        }
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
                // install the single-owner Executing<->Idle edge
                // tracker for this op (see `MonitorStartControl`). The ctx is
                // credential-scoped (no pv_request), matching the watermark
                // path, so a fanout gateway can scope the upstream
                // suspend/resume to the firing credential's cache layer.
                let start_ctl = Arc::new(MonitorStartControl::new(
                    source.clone(),
                    ch.name.clone(),
                    crate::server_native::source::ChannelContext {
                        peer,
                        account: cred.account.clone(),
                        method: cred.method.clone(),
                        host: cred.host.clone(),
                        authority: cred.authority.clone(),
                        roles: cred.roles.clone(),
                        pv_request: None,
                    },
                    monitor_exec_tx,
                ));
                if let Some(s) = ch.ops.get_mut(&ioid) {
                    s.monitor_started = true;
                    s.monitor_abort = Some(Arc::new(AbortOnDrop(join.abort_handle())));
                    s.monitor_start_ctl = Some(start_ctl.clone());
                    // Fire the initial Idle->Executing edge (pvxs
                    // onStart(true) at MONITOR START) now that the op owns
                    // the control.
                    start_ctl.set(true);
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
            let join = tokio::spawn(async move {
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
                let result = catch_handler_panic(src.rpc_checked(
                    rpc_checked,
                    req_desc,
                    req_value,
                    rpc_ctx_val,
                ))
                .await
                .map_err(|e| OpError::failed(e))
                .and_then(|r| r);

                let mut payload = Vec::new();
                payload.put_u32(ioid, order);
                // pvxs `serverget.cpp:83` echoes the request subcmd.
                payload.put_u8(subcmd);
                // `replied_ok` mirrors the status word written below; pvxs
                // `ServerGPR::doReply` cleans up a last_request RPC only after
                // success and otherwise returns it to Idle (serverget.cpp:86-116).
                let replied_ok = match result {
                    Ok((resp_desc, resp_value)) => {
                        Status::ok().write_into(order, &mut payload);
                        // Spawned task cannot hold &mut EncodeTypeCache; use inline
                        // encode_type_desc (no cache) for RPC responses.
                        encode_type_desc(&resp_desc, order, &mut payload);
                        encode_pv_field(&resp_value, &resp_desc, order, &mut payload);
                        true
                    }
                    Err(msg) => {
                        Status::error(msg).write_into(order, &mut payload);
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
            finish_exec_data_task(ch, ioid, subcmd, join.abort_handle());
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
        FieldDesc::Variant,
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
    let join = tokio::spawn(async move {
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
    finish_exec_data_task(chan_mut, ioid, 0, join.abort_handle());
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
    msg: &str,
    order: ByteOrder,
) -> PvaResult<()> {
    let buf = build_op_error_frame(kind, ioid, subcmd, msg, order);
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
    msg: &str,
    order: ByteOrder,
) -> PvaResult<()> {
    let buf = build_op_error_frame(kind, ioid, subcmd, msg, order);
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
    msg: &str,
    order: ByteOrder,
) -> Vec<u8> {
    let cmd = kind.command();
    let mut payload = Vec::new();
    payload.put_u32(ioid, order);
    payload.put_u8(subcmd);
    Status::error(msg.to_string()).write_into(order, &mut payload);
    let h = PvaHeader::application(true, order, cmd.code(), payload.len() as u32);
    let mut buf = Vec::new();
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    buf
}

/// Encode a single `CMD_MESSAGE` frame (`ioid`, `messageType`,
/// `message`). pvxs `ServerConn::logRemote()` (`serverconn.cpp:146-160`)
/// sends operation diagnostics this way. The payload layout mirrors the
/// client decoder [`crate::client_native::server_conn`] (and is
/// `ioid:u32 + mtype:u8 + message:string`); the client logs `Warning`
/// (1) at `warn!` and `Fatal` (3) at `error!`. Used by the MONITOR INIT
/// owner to surface [`MonitorOptionDiag`] negotiation diagnostics.
/// Regression R0604-PVASRV-MONITOR-OPTION-REMOTE-LOG-1.
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

#[allow(unused_imports)]
use crate::proto::ReadExt;
const _: u8 = PVA_VERSION;

/// single owner of the monitor pause/hold/squash transition.
///
/// Both the decoded and the raw-frame monitor forward loops acquire
/// their next event through this helper so the pause gate is enforced
/// in exactly one place. Semantics (mirroring pvxs keeping posts in the
/// monitor queue while Idle and draining on START):
///
/// - **Not paused**: a value held from a prior pause is returned first
///   (drain-on-resume), otherwise the next `rx.recv()` is awaited.
/// - **Paused**: keep receiving and squash to the latest into `*held`
///   without returning (no wire frame is produced), waking on
///   `resume.notified()` to flush the held value on the next iteration.
/// - **Boundary**: an event for which `is_boundary` is true is returned
///   immediately even while paused — a subscription boundary (raw
///   `type_changed` descriptor change) must tear the stream down rather
///   than be squashed behind a later, descriptor-incompatible event.
///
/// Returns `None` when the source channel closes (caller breaks/ends).
async fn next_monitor_event<T>(
    rx: &mut mpsc::Receiver<T>,
    held: &mut Option<T>,
    paused: &std::sync::atomic::AtomicBool,
    resume: &tokio::sync::Notify,
    is_boundary: impl Fn(&T) -> bool,
    // combine an already-held event with a newer one when
    // multiple events squash into `held` during a pause. The raw path
    // keeps the latest frame (`|_old, new| new`); the cooked path
    // unions the per-event marked-leaf sets so a coalesced burst still
    // marks every field that changed across it.
    coalesce: impl Fn(T, T) -> T,
) -> Option<T> {
    loop {
        if paused.load(Ordering::Relaxed) {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Some(v) if is_boundary(&v) => return Some(v),
                    Some(v) => {
                        *held = Some(match held.take() {
                            Some(old) => coalesce(old, v),
                            None => v,
                        });
                    }
                    None => return None,
                },
                _ = resume.notified() => {}
            }
        } else if let Some(v) = held.take() {
            return Some(v);
        } else {
            return rx.recv().await;
        }
    }
}

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

/// build a MONITOR data frame whose wire changed-bitset is
/// narrowed to exactly the leaves that differ between `prev` and
/// `value`, intersected with the request `mask`.
///
/// This is the partial-update counterpart of [`build_monitor_payload`]
/// — used for sources that emit partial updates (QSRV group monitor
/// with a self-trigger). pvxs `servermon.cpp:174`
/// `to_wire_valid(R, ent, &pvMask)` encodes the queued Value's own
/// marked-changed bitset intersected with the request mask; here the
/// marked set is reconstructed by structurally diffing consecutive
/// snapshots ([`crate::pvdata::encode::diff_changed_bitset`]).
///
/// When the diff is empty (no leaf changed but the source still
/// posted — e.g. an alarm-only re-post that decoded identically) the
/// frame still carries an empty changed-bitset and no value bytes,
/// matching pvxs posting an unmarked Value.
fn build_monitor_payload_partial(
    ioid: u32,
    intro: &FieldDesc,
    value: &PvField,
    prev: &PvField,
    mask: &BitSet,
    order: ByteOrder,
) -> Vec<u8> {
    // Leaves that actually changed since the last emitted snapshot.
    let diff = crate::pvdata::encode::diff_changed_bitset(intro, prev, value);
    // Intersect the diff with the request selection mask so a client
    // that subscribed to a field subset never sees leaves outside it
    // (pvxs intersects with `pvMask`).
    let mut selected = BitSet::new();
    for bit in diff.iter() {
        if mask.get(bit) {
            selected.set(bit);
        }
    }

    let mut payload = Vec::new();
    payload.put_u32(ioid, order);
    payload.put_u8(0x00);
    // Canonicalize so a fully-selected subtree collapses to its
    // structure bit exactly as `build_monitor_payload` does.
    let changed = crate::pvdata::encode::canonical_changed_bitset(intro, &selected);
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

/// build a MONITOR data frame whose wire changed-bitset
/// is the explicit set of `marked_paths` (a QSRV group `+trigger`
/// target set), intersected with the request `mask`.
///
/// Unlike [`build_monitor_payload_partial`], which reconstructs the
/// marked set by diffing consecutive snapshots and so only marks
/// leaves that *changed*, this marks each target field whether or not
/// its value differs — pvxs `groupsource.cpp:288` refreshes and marks
/// every `+trigger` target (assigned-not-changed).
fn build_monitor_payload_marked(
    ioid: u32,
    intro: &FieldDesc,
    value: &PvField,
    marked_paths: &[String],
    mask: &BitSet,
    order: ByteOrder,
) -> Vec<u8> {
    // Selection = every leaf under each marked target path.
    let target = crate::pvdata::encode::marked_changed_bitset(intro, marked_paths);
    // Intersect with the request mask so a client that subscribed to a
    // field subset never sees leaves outside it (pvxs intersects with
    // `pvMask`).
    let mut selected = BitSet::new();
    for bit in target.iter() {
        if mask.get(bit) {
            selected.set(bit);
        }
    }

    let mut payload = Vec::new();
    payload.put_u32(ioid, order);
    payload.put_u8(0x00);
    let changed = crate::pvdata::encode::canonical_changed_bitset(intro, &selected);
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

/// coalesce two cooked monitor updates when the server
/// squashes events under back-pressure or pause. The newer value wins;
/// the marked-leaf sets union so the emitted frame still marks every
/// field that changed across the coalesced burst. A `None` on either
/// side means "no explicit set — derive by diff", which over-marks
/// safely, so the union of `None` with anything stays `None`.
///
/// A `type_changed` boundary must SURVIVE the squash: once the upstream
/// descriptor changed, no value (squashed-old or post-boundary-new) may
/// be delivered under the negotiated descriptor, so if either side is a
/// boundary the result is the boundary — the dispatch loop then emits
/// MONITOR FINISH instead of encoding a stale-descriptor value. This is
/// what keeps the decoded type-change marker from being lost when the
/// pause-hold or the `drain_monitor_queue` overflow coalesces a burst.
fn coalesce_monitor_update(
    older: crate::server_native::MonitorUpdate,
    newer: crate::server_native::MonitorUpdate,
) -> crate::server_native::MonitorUpdate {
    if older.type_changed || newer.type_changed {
        return crate::server_native::MonitorUpdate::type_change();
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
    }
}

/// Fold the in-hand monitor update and every immediately-available update
/// from `rx` into the bounded server-side queue, returning the
/// (possibly squashed) front as the next event to consider plus a flag
/// set when any overflow squash occurred.
///
/// This is the server monitor queue, modelled after pvxs
/// `servermon.cpp:271-287`: a post is appended as a DISTINCT queue entry
/// while the queue holds fewer than `limit` entries, and only once the
/// queue is full is the newest update squashed into the queue tail
/// (unioning marked-leaf sets via [`coalesce_monitor_update`]). The send
/// loop pops exactly one entry per reply (`servermon.cpp:171-183`), so a
/// burst below the negotiated depth is delivered as distinct DATA frames
/// rather than collapsed into one. Coalescing is the OVERFLOW action, not
/// the normal drain.
///
/// `in_hand` is the front of the conceptual queue — pvxs counts
/// `queue.front()` (the post being prepared for the next reply) against
/// `queueSize` and pops it only in `doReply()` when a frame is actually
/// produced. So this helper is the SINGLE owner of the unsent count:
/// `[in_hand] ++ pending` is bounded to `limit` total, never `limit + 1`.
/// `in_hand` is pushed to the front, the queue is bounded, then the front
/// is restored as the returned value — at `limit == 1` the front IS the
/// tail, so an overflow squashes into `in_hand` itself.
///
/// `limit` must be >= 1, so the queue always has a tail to squash into.
fn drain_monitor_queue(
    rx: &mut mpsc::Receiver<crate::server_native::MonitorUpdate>,
    in_hand: crate::server_native::MonitorUpdate,
    pending: &mut std::collections::VecDeque<crate::server_native::MonitorUpdate>,
    limit: usize,
) -> (crate::server_native::MonitorUpdate, bool) {
    // Fold the in-hand front into the SAME queue so one uniform rule
    // covers every unsent post: it extends the queue while there is room,
    // else squashes into the tail.
    pending.push_front(in_hand);
    let mut overflow = false;
    // `try_recv` Empty (no more buffered) or Disconnected (source ended)
    // both stop the drain; disconnect is observed by the outer loop's
    // `next_monitor_event` returning None once the queue empties.
    while let Ok(next) = rx.try_recv() {
        if pending.len() < limit {
            pending.push_back(next);
        } else {
            let tail = pending
                .pop_back()
                .expect("push_front above guarantees a non-empty queue to squash into");
            pending.push_back(coalesce_monitor_update(tail, next));
            overflow = true;
        }
    }
    // Restore the (possibly squashed) front as the next in-hand event.
    let front = pending
        .pop_front()
        .expect("push_front above guarantees a non-empty queue");
    (front, overflow)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_native::decode::{OpResponse, decode_op_response, try_parse_frame};
    use crate::pvdata::{PvStructure, ScalarType, ScalarValue};

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
    /// [`WatermarkKind::Withdraw`] carrying this op's `op_id` (the gateway
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
            async fn subscribe(&self, _name: &str) -> Option<tokio::sync::mpsc::Receiver<PvField>> {
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
    /// fix `pv_value_leaf_to_epics`'s `array` helper had no `ULong`
    /// arm, so a wire-decoded `ScalarArrayTyped::ULong` fell through
    /// to `None`; `pv_field_to_filter_event` then substituted a
    /// scalar `Double(0.0)`, and a filtered `DBF_UINT64` waveform was
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
            pv_value_leaf_to_epics(&typed),
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
            for sv in [
                ScalarValue::Boolean(true),
                ScalarValue::Byte(-3),
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
            let cases = [
                vec![ScalarValue::UInt(1), ScalarValue::UInt(2)],
                vec![ScalarValue::UShort(1)],
                vec![ScalarValue::Boolean(true)],
                vec![ScalarValue::Byte(-1)],
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

    /// Boundary test for the server-side monitor queue
    /// ([`drain_monitor_queue`], pvxs servermon.cpp:271-287). The drain is
    /// the SINGLE owner of the unsent count: it folds the in-hand front
    /// event into the same bounded queue, so `[in_hand] ++ pending` never
    /// exceeds `limit`. Tested by invariant boundary — below limit, at
    /// limit, the `limit == 1` squash-into-front, and a boundary post — not
    /// by a narrative burst.
    ///
    /// Regression R0604-PVASRV-MONITOR-QUEUE-OFFBYONE-1: before the fix the
    /// in-hand `value` was held OUTSIDE the queue and only `pending.len()`
    /// was bounded, so under backpressure `queue_limit + 1` distinct posts
    /// went unsent. This reproduces the real loop state the old isolated
    /// helper test missed (one event already removed from the queue before
    /// the drain): with `record[queueSize=2]`, in-hand `first` plus posts
    /// `[second, third]` must leave exactly two unsent events `[first,
    /// third]`, not three.
    #[tokio::test]
    async fn pva_44_monitor_queue_counts_in_hand_event_against_limit() {
        use std::collections::VecDeque;
        let val = |tag: i32| PvField::Scalar(ScalarValue::Int(tag));
        let upd = |tag: i32| crate::server_native::MonitorUpdate {
            value: val(tag),
            marked: None,
            type_changed: false,
        };
        // The whole unsent set the loop will deliver, in send order: the
        // returned front followed by the rest of the bounded queue.
        let unsent =
            |front: &crate::server_native::MonitorUpdate,
             pending: &VecDeque<crate::server_native::MonitorUpdate>| {
                let mut v = vec![front.value.clone()];
                v.extend(pending.iter().map(|u| u.value.clone()));
                v
            };

        // The finding's exact loop state: record[queueSize=2], pipeline
        // credit at zero so nothing is sent, in-hand `first` already popped
        // for this iteration, posts `second` and `third` buffered on the
        // channel. The whole unsent set is bounded to 2 — [first, third],
        // the newest squashed into the tail — NOT three distinct frames.
        let (tx, mut rx) = mpsc::channel::<crate::server_native::MonitorUpdate>(16);
        for tag in [2, 3] {
            tx.send(upd(tag)).await.unwrap();
        }
        let mut pending: VecDeque<crate::server_native::MonitorUpdate> = VecDeque::new();
        let (front, overflow) = drain_monitor_queue(&mut rx, upd(1), &mut pending, 2);
        assert!(
            overflow,
            "in-hand + 2 posts into limit 2 must squash the tail"
        );
        assert_eq!(
            unsent(&front, &pending),
            vec![val(1), val(3)],
            "queueSize=2 holds the in-hand front plus one queued tail, \
             squashing the newest into it — never queue_limit + 1 distinct posts"
        );

        // Below the limit: the in-hand front plus the queued posts all stay
        // distinct (in-hand counts against the limit, but there is room).
        let (tx, mut rx) = mpsc::channel::<crate::server_native::MonitorUpdate>(16);
        for tag in [2, 3] {
            tx.send(upd(tag)).await.unwrap();
        }
        let mut pending: VecDeque<crate::server_native::MonitorUpdate> = VecDeque::new();
        let (front, overflow) = drain_monitor_queue(&mut rx, upd(1), &mut pending, 4);
        assert!(!overflow, "in-hand + 2 posts below limit 4 must not squash");
        assert_eq!(
            unsent(&front, &pending),
            vec![val(1), val(2), val(3)],
            "in-hand front and queued posts stay distinct below the limit"
        );

        // queueSize == 1: the front IS the tail, so every later post
        // squashes into the in-hand event itself (latest value wins).
        let (tx, mut rx) = mpsc::channel::<crate::server_native::MonitorUpdate>(16);
        for tag in [2, 3] {
            tx.send(upd(tag)).await.unwrap();
        }
        let mut pending: VecDeque<crate::server_native::MonitorUpdate> = VecDeque::new();
        let (front, overflow) = drain_monitor_queue(&mut rx, upd(1), &mut pending, 1);
        assert!(overflow, "limit 1 squashes every later post into the front");
        assert!(
            pending.is_empty(),
            "limit 1 leaves no distinct queue entries"
        );
        assert_eq!(
            unsent(&front, &pending),
            vec![val(3)],
            "limit 1 collapses in-hand + posts to the single newest value"
        );

        // A descriptor-change boundary among the posts is sticky through the
        // squash: at limit 1 it coalesces into the front, which the loop
        // then detects (MONITOR FINISH) rather than encoding a stale value.
        let (tx, mut rx) = mpsc::channel::<crate::server_native::MonitorUpdate>(16);
        tx.send(upd(2)).await.unwrap();
        tx.send(crate::server_native::MonitorUpdate::type_change())
            .await
            .unwrap();
        let mut pending: VecDeque<crate::server_native::MonitorUpdate> = VecDeque::new();
        let (front, _overflow) = drain_monitor_queue(&mut rx, upd(1), &mut pending, 1);
        assert!(
            front.type_changed,
            "a boundary post must survive the limit-1 squash into the front"
        );
    }

    /// Owner: invariant boundaries of [`next_monitor_event`],
    /// the single owner of the monitor pause/hold/squash transition that
    /// both the decoded and the raw-frame forward loops acquire through.
    /// Tested by boundary (paused vs not, held empty vs set, boundary vs
    /// not, channel open vs closed), not by narrative scenario.
    mod next_monitor_event_owner {
        use super::*;
        use std::sync::atomic::AtomicBool;
        use tokio::sync::Notify;

        /// Not paused, nothing held → the next received value is yielded.
        #[tokio::test]
        async fn not_paused_yields_next_recv() {
            let (tx, mut rx) = mpsc::channel::<u32>(8);
            let paused = AtomicBool::new(false);
            let resume = Notify::new();
            let mut held: Option<u32> = None;
            tx.send(7).await.unwrap();
            let got =
                next_monitor_event(&mut rx, &mut held, &paused, &resume, |_| false, |_, n| n).await;
            assert_eq!(got, Some(7));
        }

        /// Not paused, a value held from a prior pause → the held value
        /// drains first (resume flush) before any new recv.
        #[tokio::test]
        async fn not_paused_drains_held_first() {
            let (tx, mut rx) = mpsc::channel::<u32>(8);
            let paused = AtomicBool::new(false);
            let resume = Notify::new();
            let mut held: Option<u32> = Some(42);
            // A fresher value is queued, but the held one must win.
            tx.send(99).await.unwrap();
            let got =
                next_monitor_event(&mut rx, &mut held, &paused, &resume, |_| false, |_, n| n).await;
            assert_eq!(got, Some(42), "held value drains before fresh recv");
            assert!(held.is_none(), "held is consumed once drained");
        }

        /// Paused → events squash to the latest into `held` without
        /// returning; resume flushes only the latest (no per-event frame).
        #[tokio::test]
        async fn paused_squashes_to_latest_then_resume_flushes() {
            let (tx, mut rx) = mpsc::channel::<u32>(8);
            let paused = Arc::new(AtomicBool::new(true));
            let resume = Arc::new(Notify::new());
            let p2 = paused.clone();
            let r2 = resume.clone();
            let task = tokio::spawn(async move {
                let mut held: Option<u32> = None;
                next_monitor_event(&mut rx, &mut held, &p2, &r2, |_| false, |_, n| n).await
            });
            // Feed while paused; each yield lets the owner squash the
            // sent value into `held` and park again in the select.
            for v in [1u32, 2, 3] {
                tx.send(v).await.unwrap();
                tokio::task::yield_now().await;
            }
            paused.store(false, Ordering::Relaxed);
            resume.notify_one();
            assert_eq!(
                task.await.unwrap(),
                Some(3),
                "paused squash yields only the latest value on resume"
            );
        }

        /// Paused + a boundary event → the boundary is yielded
        /// immediately (not squashed behind a later, descriptor-
        /// incompatible event). A non-boundary that arrived first is
        /// squashed into `held`.
        #[tokio::test]
        async fn paused_yields_boundary_immediately() {
            use crate::proto::ByteOrder;
            use crate::server_native::RawMonitorEvent;

            let (tx, mut rx) = mpsc::channel::<RawMonitorEvent>(8);
            let paused = AtomicBool::new(true);
            let resume = Notify::new();
            let mut held: Option<RawMonitorEvent> = None;
            tx.send(RawMonitorEvent {
                body_bytes: bytes::Bytes::from_static(b"stale"),
                byte_order: ByteOrder::Little,
                type_changed: false,
            })
            .await
            .unwrap();
            tx.send(RawMonitorEvent {
                body_bytes: bytes::Bytes::new(),
                byte_order: ByteOrder::Little,
                type_changed: true,
            })
            .await
            .unwrap();
            let got = next_monitor_event(
                &mut rx,
                &mut held,
                &paused,
                &resume,
                |ev| ev.type_changed,
                |_, n| n,
            )
            .await;
            assert!(
                got.is_some_and(|e| e.type_changed),
                "boundary event must be yielded immediately even while paused"
            );
            assert!(
                held.is_some_and(|e| !e.type_changed),
                "the pre-boundary non-boundary event was squashed into held"
            );
        }

        /// Source channel closed → `None` (caller ends the stream).
        #[tokio::test]
        async fn closed_channel_yields_none() {
            let (tx, mut rx) = mpsc::channel::<u32>(1);
            let paused = AtomicBool::new(false);
            let resume = Notify::new();
            let mut held: Option<u32> = None;
            drop(tx);
            let got =
                next_monitor_event(&mut rx, &mut held, &paused, &resume, |_| false, |_, n| n).await;
            assert_eq!(got, None);
        }
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

    /// Unwrap the parsed options, asserting the request was NOT a
    /// pipeline-negotiation reject.
    fn parsed_opts(req: &PvField) -> PipelineOptions {
        match monitor_pipeline_options(req) {
            Some(MonitorPipelineRequest::Options(o)) => o,
            Some(MonitorPipelineRequest::Reject) => {
                panic!("expected parsed options, got a pipeline-negotiation Reject")
            }
            None => panic!("expected parsed options, got None (no _options structure)"),
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
            matches!(
                monitor_pipeline_options(&req),
                Some(MonitorPipelineRequest::Reject)
            ),
            "pipeline + queueSize<2 must reject the INIT, not downgrade",
        );
    }

    #[test]
    fn pva_r20_pipeline_unparseable_queue_size_rejects() {
        // PRESENT but unparseable queueSize under pipeline → Reject
        // (pvxs `queueSize.as(qSize)` fails, then `op->pipeline` → error).
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::String("not-a-number".into())),
        );
        assert!(
            matches!(
                monitor_pipeline_options(&req),
                Some(MonitorPipelineRequest::Reject)
            ),
            "pipeline + unparseable queueSize must reject the INIT",
        );
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

        /// No value body (cursor at end after the descriptor): tolerated
        /// as "no options".
        #[test]
        fn absent_body_is_none() {
            let desc = FieldDesc::Scalar(ScalarType::Int);
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
    #[tokio::test]
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
                    introspection: Some(intro),
                    source,
                    stat: crate::server_native::peers::ChannelStat::new(String::new()),
                    ops: HashMap::new(),
                },
            );
            let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
            let config = PvaServerConfig::default();
            let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
            let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
            let cred = ClientCredentials::anonymous();

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
        // Explicit 0 → queueSize/2 (servermon.cpp:577-578).
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(q as i32)),
            PvField::Scalar(ScalarValue::Int(0)),
        );
        assert_eq!(parsed_opts(&req).ack_at, q / 2, "ackAny=0 → queueSize/2");
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
        // A percent so small it truncates below one slot becomes 0,
        // which hits the explicit `ackAt==0 → limit/2` fallback
        // (pvxs `servermon.cpp:577`), NOT the `[1, limit]` floor:
        // 0.5% of 16 = 0.08 → 0 → limit/2 = 8.
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(16)),
            PvField::Scalar(ScalarValue::String("0.5%".into())),
        );
        assert_eq!(
            parsed_opts(&req).ack_at,
            8,
            "0.5% of 16 = 0.08 → truncates to 0 → limit/2 fallback = 8"
        );
    }

    // ---- R0604-PVASRV-MONITOR-OPTION-REMOTE-LOG-1 ----
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
    fn monitor_diag_recognized_false_tokens_do_not_warn() {
        // Recognized false-ish tokens are intentional disables, NOT parse
        // errors — no diagnostic (only genuinely unrecognized values warn).
        for tok in ["false", "0", "no", "FALSE", "No"] {
            let req = make_pipeline_request(
                PvField::Scalar(ScalarValue::String(tok.into())),
                PvField::Scalar(ScalarValue::Int(16)),
            );
            let opts = parsed_opts(&req);
            assert!(!opts.enabled, "{tok} → disabled");
            assert!(
                opts.diagnostics.is_empty(),
                "{tok} is a recognized false token, no Warn"
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

    #[test]
    fn monitor_diag_ackany_non_scalar_under_pipeline_is_crit() {
        // pvxs `servermon.cpp:570-573`: a NON-scalar ackAny fails both
        // `as<int>` and `as<string>` → Crit logRemote. ackAt stays default.
        let non_scalar = PvField::Structure(PvStructure {
            struct_id: String::new(),
            fields: vec![],
        });
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(16)),
            non_scalar,
        );
        let opts = parsed_opts(&req);
        assert!(opts.enabled);
        assert_eq!(opts.ack_at, 1, "non-scalar ackAny leaves ackAt default");
        assert_eq!(opts.diagnostics.len(), 1, "one ackAny Crit");
        assert_eq!(opts.diagnostics[0].level, MessageType::Fatal);
        assert!(
            opts.diagnostics[0].message.contains("ackAny"),
            "message names the ackAny option: {}",
            opts.diagnostics[0].message
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

    /// Emission half of R0604-PVASRV-MONITOR-OPTION-REMOTE-LOG-1: a
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
                introspection: Some(intro.clone()),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous();

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
        }
    }

    /// Owner path: `MonitorPipelineCredit::acquire` consumes exactly one
    /// window slot per call.
    #[tokio::test]
    async fn monitor_pipeline_credit_acquire_decrements_window() {
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
        credit.acquire().await;
        assert_eq!(window.load(Ordering::Relaxed), 1);
        credit.acquire().await;
        assert_eq!(window.load(Ordering::Relaxed), 0);
    }

    /// Owner path: at zero credit `acquire` blocks, and proceeds once the
    /// window is refilled (the ACK dispatch does `fetch_add` + notify).
    #[tokio::test]
    async fn monitor_pipeline_credit_acquire_blocks_until_refill() {
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
        // Exhausted window: acquire must not complete.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), credit.acquire())
                .await
                .is_err(),
            "acquire must block while the window is empty"
        );
        // Refill (as the ACK dispatch does) and re-acquire.
        window.fetch_add(1, Ordering::Relaxed);
        notify.notify_waiters();
        assert!(
            tokio::time::timeout(Duration::from_millis(500), credit.acquire())
                .await
                .is_ok(),
            "acquire must complete once the window is refilled"
        );
        assert_eq!(window.load(Ordering::Relaxed), 0);
    }

    /// Owner path: a non-pipeline monitor (no window) never blocks and
    /// touches no counter.
    #[tokio::test]
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
        tokio::time::timeout(Duration::from_millis(50), credit.acquire())
            .await
            .expect("non-pipeline acquire must return immediately");
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
                introspection: Some(intro.clone()),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous();

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
                introspection: Some(intro.clone()),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous();

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
                introspection: Some(intro.clone()),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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
        let cred = ClientCredentials::anonymous();

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
            op.monitor_options.queue_size,
            Some(2),
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
                introspection: Some(intro.clone()),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous();

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

        let started = |chs: &HashMap<u32, ChannelState>| -> bool {
            chs.get(&sid)
                .and_then(|c| c.ops.get(&ioid))
                .map(|o| o.monitor_started)
                .expect("op present")
        };
        assert!(!started(&channels), "monitor must be idle right after INIT");

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
                intro: FieldDesc::Variant,
                kind: OpKind::Monitor,
                monitor_started: true,
                monitor_abort: None,
                mask: BitSet::new(),
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
                put_auto_exec: true,
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
    #[tokio::test]
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
        let cred = ClientCredentials::anonymous();

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
    #[tokio::test]
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
        let cred = ClientCredentials::anonymous();

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
    #[tokio::test]
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
        let cred = ClientCredentials::anonymous();

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

    /// Recorder source for the MONITOR ACK validate-order regression
    /// (R0604-PVASRV-MONITOR-ACK-VALIDATE-ORDER-1). Pushes every
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
        async fn subscribe(&self, _n: &str) -> Option<mpsc::Receiver<PvField>> {
            let (_tx, rx) = mpsc::channel(1);
            Some(rx)
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
            intro.clone(),
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
                introspection: Some(intro.clone()),
                source: src.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                ops,
            },
        );
        channels
    }

    /// Regression R0604-PVASRV-MONITOR-ACK-VALIDATE-ORDER-1.
    ///
    /// A truncated `0xC4` (ACK|START) frame with no ACK `u32` must be a
    /// connection-fatal Decode error with NO START side effect: `monitor_paused`
    /// stays paused and `notify_monitor_start` never fires. pvxs
    /// (servermon.cpp:599-608) reads/validates the ACK count before any op
    /// lookup or `onStart`. Pre-fix the START block ran first, clearing the
    /// pause and firing `notify_monitor_start(true)` before the decode error.
    #[tokio::test]
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
        let cred = ClientCredentials::anonymous();

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

    /// Regression R0604-PVASRV-MONITOR-ACK-VALIDATE-ORDER-1.
    ///
    /// A truncated `0x84` (ACK|STOP) frame with no ACK `u32` must be a
    /// connection-fatal Decode error with NO STOP side effect: `monitor_paused`
    /// stays cleared and `notify_monitor_start(false)` never fires. Pre-fix the
    /// STOP block stored `monitor_paused=true` and fired the stop edge before
    /// the decode error.
    #[tokio::test]
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
        let cred = ClientCredentials::anonymous();

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

    /// Regression R0604-PVASRV-MONITOR-ACK-VALIDATE-ORDER-1.
    ///
    /// A well-formed `0xC4` (ACK|START) refills the window AND resumes, and
    /// pvxs (servermon.cpp:643-689) applies ACK refill THEN START, so the
    /// `onHighMark` (Resume watermark) precedes `onStart`. Pre-fix the START
    /// block ran first, reversing the order to [start, watermark].
    #[tokio::test]
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
        let cred = ClientCredentials::anonymous();

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
                FieldDesc::Scalar(ScalarType::Int),
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
                introspection: Some(FieldDesc::Scalar(ScalarType::Int)),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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

    /// Regression R0604-PVASRV-MESSAGE-SEVERITY-MAP-1.
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
                FieldDesc::Scalar(ScalarType::Int),
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
                introspection: Some(FieldDesc::Scalar(ScalarType::Int)),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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

    #[tokio::test]
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
                intro: FieldDesc::Variant,
                kind: OpKind::Monitor,
                monitor_started: true,
                monitor_abort: Some(abort.clone()),
                mask: BitSet::new(),
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
                put_auto_exec: true,
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
    #[tokio::test]
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
        let mut op = non_monitor_op_state(FieldDesc::Variant, OpKind::Get, BitSet::new());
        op.exec_state = ExecState::Executing;
        op.data_task_abort = Some(abort);
        op.last_request = true; // a last-request EXEC that is now cancelled
        let old_op_id = op.monitor_op_id;
        // Regression R0604-PVASRV-CANCEL-LASTREQUEST-1: the sticky destroy
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
                introspection: Some(FieldDesc::Variant),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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

    /// Regression R0604-PVASRV-CANCEL-LASTREQUEST-1. pvxs `CANCEL_REQUEST`
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
        let mut op = non_monitor_op_state(FieldDesc::Variant, OpKind::Get, BitSet::new());
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
                introspection: Some(FieldDesc::Variant),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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

    /// Residual regression: `build_monitor_payload_partial`
    /// narrows the wire changed-bitset to exactly the leaves that
    /// differ from the previous snapshot, intersected with the
    /// request mask — pvxs `servermon.cpp:174`
    /// `to_wire_valid(R, ent, &pvMask)`. The previous QSRV group
    /// monitor path always sent the full request mask, so a
    /// self-trigger update on one member wrongly marked every member
    /// changed on the wire.
    ///
    /// This builds a two-member group value and an event in which
    /// only member `a` changed; the decoded frame's changed-bitset
    /// must mark `a`'s leaf and NOT `b`'s. The contrasting full-mask
    /// `build_monitor_payload` marks both — proving the narrowing is
    /// the partial builder's doing.
    #[test]
    fn br_r29_partial_monitor_payload_narrows_changed_bitset() {
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
        // Previous emitted snapshot, then an event where only `a`
        // changed (self-trigger on member `a`).
        let prev = mk(1.0, 2.0);
        let curr = mk(9.0, 2.0);
        let mask = BitSet::all_set(intro.total_bits());

        // Partial builder: changed-bitset must mark only `a` (bit 1),
        // not `b` (bit 2).
        let partial = build_monitor_payload_partial(ioid, &intro, &curr, &prev, &mask, order);
        let (frame, _) = try_parse_frame(&partial).unwrap().expect("complete frame");
        let data = match decode_op_response(&frame, Some(&intro)).unwrap() {
            OpResponse::Data(d) => d,
            other => panic!("expected monitor data, got {other:?}"),
        };
        assert!(
            data.changed.get(1),
            "member `a` (bit 1) changed — must be marked"
        );
        assert!(
            !data.changed.get(2),
            "member `b` (bit 2) unchanged — must NOT be marked (narrowing)"
        );

        // Full builder: marks both members — confirms the old
        // behaviour the residual gap describes.
        let full = build_monitor_payload(ioid, &intro, &curr, &mask, order);
        let (full_frame, _) = try_parse_frame(&full).unwrap().expect("complete frame");
        let full_data = match decode_op_response(&full_frame, Some(&intro)).unwrap() {
            OpResponse::Data(d) => d,
            other => panic!("expected monitor data, got {other:?}"),
        };
        assert!(
            full_data.changed.get(1) && full_data.changed.get(2),
            "full-mask builder marks every member — the pre-fix wire shape"
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

        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous();
        let peer_entry = crate::server_native::peers::PeerEntry::new(false);
        let teardown = ChannelTeardownCtx {
            tx: &tx,
            order,
            peer,
            cred: &cred,
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
    #[tokio::test]
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
                ops: HashMap::new(),
            },
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);

        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(cid, order);
        let frame = synth_frame(Command::DestroyChannel, order, payload);

        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous();
        let peer_entry = crate::server_native::peers::PeerEntry::new(false);
        let teardown = ChannelTeardownCtx {
            tx: &tx,
            order,
            peer,
            cred: &cred,
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
    #[tokio::test]
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
        let cred = ClientCredentials::anonymous();
        let peer_entry = crate::server_native::peers::PeerEntry::new(false);
        // Server configured little-endian (config_order) for its OWN
        // outbound frames; the inbound frame is big-endian.
        let teardown = ChannelTeardownCtx {
            tx: &tx,
            order: config_order,
            peer,
            cred: &cred,
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
    #[tokio::test]
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
        let cred = ClientCredentials::anonymous();

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
    #[tokio::test]
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
            async fn subscribe(&self, _name: &str) -> Option<tokio::sync::mpsc::Receiver<PvField>> {
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
                ops: HashMap::new(),
            },
        );
        let (tx, mut _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);

        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(cid, order);
        let frame = synth_frame(Command::DestroyChannel, order, payload);

        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous();
        let peer_entry = crate::server_native::peers::PeerEntry::new(false);
        let teardown = ChannelTeardownCtx {
            tx: &tx,
            order,
            peer,
            cred: &cred,
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

    /// A source's out-of-band channel invalidation (PVA gateway
    /// operator `:drop`/`:flush`) force-disconnects exactly the downstream
    /// channels serving the named PV with a server-initiated DESTROY_CHANNEL,
    /// leaving channels under other names untouched. Before this fix a
    /// drop/flush only ended the upstream monitor; the downstream channel
    /// lingered and silently rebound to a re-created cache entry on the next
    /// event. Asserts the single-owner teardown removed only the match, sent
    /// a DESTROY_CHANNEL addressing it, and released only its report count.
    #[tokio::test]
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
        let cred = ClientCredentials::anonymous();
        let teardown = ChannelTeardownCtx {
            tx: &tx,
            order,
            peer,
            cred: &cred,
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
    #[tokio::test]
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
                ops: HashMap::new(),
            },
        );
        peer_entry.channel_opened(1, stat);

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous();
        let teardown = ChannelTeardownCtx {
            tx: &tx,
            order,
            peer,
            cred: &cred,
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
                introspection: Some(intro.clone()),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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
            &init_frame,
            &tx,
            &mut channels,
            order,
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
                introspection: Some(intro.clone()),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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
            &init_frame,
            &tx,
            &mut channels,
            order,
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
    #[tokio::test]
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
        let cred = ClientCredentials::anonymous();

        // A channel carrying one already-INIT'd GET op.
        let make_channels = || {
            let mut ops: HashMap<u32, OpState> = HashMap::new();
            ops.insert(
                ioid,
                non_monitor_op_state(
                    intro.clone(),
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
                    introspection: Some(intro.clone()),
                    source: source.clone(),
                    stat: crate::server_native::peers::ChannelStat::new(String::new()),
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
    #[tokio::test]
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
                introspection: Some(intro.clone()),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous();

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
                introspection: Some(intro.clone()),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                ops: HashMap::new(),
            },
        );
        let mut fresh_cache = TypeCache::new();
        let err = handle_op(
            &ref_frame,
            &tx,
            &mut empty_channels,
            order,
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
    #[tokio::test]
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
            async fn subscribe(&self, _: &str) -> Option<mpsc::Receiver<PvField>> {
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
        };

        // Delta marking only field `b` (bit 2) -> 99.
        let mut changed = BitSet::new();
        changed.set(2);
        let delta = three_field_value(0, 99, 0);

        src.put_delta_checked(checked, three_field_intro(), changed, delta, ctx)
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
                introspection: Some(intro.clone()),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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
            &init_frame,
            &tx,
            &mut channels,
            order,
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
                introspection: Some(intro.clone()),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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
        ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
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
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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

        let creds = parse_client_credentials(&frame, &mut TypeCache::new())
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

        let parsed =
            parse_client_credentials(&frame, &mut TypeCache::new()).expect("decode must succeed");
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

        let creds = parse_client_credentials(&frame, &mut TypeCache::new())
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

        let creds = parse_client_credentials(&frame, &mut TypeCache::new())
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

        let creds = parse_client_credentials(&frame, &mut TypeCache::new())
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
    #[tokio::test]
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
    #[tokio::test]
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
        ops.insert(ioid, non_monitor_op_state(intro.clone(), kind, mask));
        let mut channels = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(intro),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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
    #[tokio::test]
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
    #[tokio::test]
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
                introspection: Some(intro),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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
    #[tokio::test]
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
    #[tokio::test]
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
    #[tokio::test]
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

    /// Regression: the dedicated `handle_put_get` data phase must
    /// reject a frame whose IOID was initialised as a different
    /// operation class (here a GET). Before the fix it extracted
    /// `(intro, mask)` from whatever op existed and performed a
    /// write/readback the operation never negotiated as a PUT_GET.
    #[tokio::test]
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
    #[tokio::test]
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
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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
    #[tokio::test]
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
    #[tokio::test]
    async fn pva_put_get_last_request_executes_then_defers_destroy() {
        let order = ByteOrder::Little;
        let (sid, ioid) = (1u32, 701u32);
        let source: DynSource = std::sync::Arc::new(AuthorityGatedSource::new());

        let intro = three_field_intro();
        let mask = BitSet::all_set(intro.total_bits());
        let mut ops = HashMap::new();
        ops.insert(
            ioid,
            non_monitor_op_state(intro.clone(), OpKind::PutGet, mask),
        );
        let mut channels = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(intro.clone()),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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
                monitor_resume: Arc::new(tokio::sync::Notify::new()),
                monitor_wm: None,
                monitor_wm_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
                monitor_op_id: next_op_id(),
                monitor_filters: Arc::new(
                    epics_base_rs::server::database::filters::FilterChain::new(),
                ),
                put_auto_exec: true,
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
                introspection: Some(FieldDesc::Variant),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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
        let cred = ClientCredentials::anonymous();
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
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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
        let cred = ClientCredentials::anonymous();
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
    /// rx charge that drops the 8-byte header) regresses this test
    /// (Regression R0604-PVASRV-REPORT-CHANNEL-BYTE-ACCOUNTING-1).
    #[tokio::test]
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
                introspection: Some(FieldDesc::Variant),
                source: source.clone(),
                stat: stat.clone(),
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
        let cred = ClientCredentials::anonymous();
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

    /// Owner math for per-channel inbound op-RX accounting
    /// (R0604-PVASRV-REPORT-CHANNEL-BYTE-ACCOUNTING-1): `add_op_rx` must
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
    /// client case over-counted (Regression
    /// R0604-PVASRV-REPORT-CHANNEL-BYTE-ACCOUNTING-1).
    #[tokio::test]
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
                    ops: HashMap::new(),
                },
            );
            let peer_entry = crate::server_native::peers::PeerEntry::new(false);
            peer_entry.channel_opened(sid, stat.clone());

            let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
            let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
            let cred = ClientCredentials::anonymous();
            let teardown = ChannelTeardownCtx {
                tx: &tx,
                order,
                peer,
                cred: &cred,
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
    #[tokio::test]
    async fn get_field_none_introspection_replies_error_no_descriptor() {
        use crate::client_native::decode::{decode_get_field_response, try_parse_frame};
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
        let cred = ClientCredentials::anonymous();
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
    #[tokio::test]
    async fn get_field_slow_path_returns_source_descriptor() {
        use crate::client_native::decode::{decode_get_field_response, try_parse_frame};
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
        let cred = ClientCredentials::anonymous();
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
    #[tokio::test]
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
            async fn subscribe(&self, _name: &str) -> Option<mpsc::Receiver<PvField>> {
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
        let cred = ClientCredentials::anonymous();

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
        async fn subscribe(&self, _n: &str) -> Option<mpsc::Receiver<PvField>> {
            // Sender dropped immediately → receiver closed → the subscriber
            // loop sees end-of-stream and the task ends (source close).
            let (_tx, rx) = mpsc::channel(1);
            Some(rx)
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
            intro.clone(),
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
                introspection: Some(intro.clone()),
                source: src.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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
    #[tokio::test]
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

        let (exec_tx, exec_rx) = tokio::sync::watch::channel(false);
        spawn_monitor_gate_driver(gate, exec_rx);
        let ctl = MonitorStartControl::new(src.clone(), "dut".into(), bfr12_anon_ctx(), exec_tx);

        async fn wait_len(log: &Arc<parking_lot::Mutex<Vec<bool>>>, n: usize) {
            for _ in 0..200 {
                if log.lock().len() >= n {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
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
                introspection: Some(intro.clone()),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let (mon_fin_tx, mut mon_fin_rx) = mpsc::unbounded_channel::<MonitorFinished>();
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous();

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
        async fn subscribe(&self, _: &str) -> Option<mpsc::Receiver<PvField>> {
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
                introspection: intro,
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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
    #[tokio::test]
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
        let cred = ClientCredentials::anonymous();

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
                    introspection: Some(three_field_intro()),
                    source: source.clone(),
                    stat: crate::server_native::peers::ChannelStat::new(String::new()),
                    ops: HashMap::new(),
                },
            );
        }
        channels
    }

    /// An IOID already live on one channel makes an INIT reusing it on a
    /// *different* channel connection-fatal — pvxs scopes IOIDs to
    /// `ServerConn::opByIOID`, not per channel (serverget.cpp:378-384).
    #[tokio::test]
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
                three_field_intro(),
                OpKind::Get,
                BitSet::all_set(three_field_intro().total_bits()),
            ),
        );

        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous();

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
    #[tokio::test]
    async fn pva_conn_wide_destroy_request_routes_by_ioid_ignoring_sid() {
        let order = ByteOrder::Little;
        let ioid = 9u32;
        let source: DynSource = Arc::new(Bfr13FailSource);
        let mut channels = two_channel_conn(source.clone());
        channels.get_mut(&1).unwrap().ops.insert(
            ioid,
            non_monitor_op_state(
                three_field_intro(),
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
    #[tokio::test]
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
        let cred = ClientCredentials::anonymous();

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
    #[tokio::test]
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
        let cred = ClientCredentials::anonymous();

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
    #[tokio::test]
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
        let mut cred = ClientCredentials::anonymous();
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
        async fn subscribe(&self, _name: &str) -> Option<tokio::sync::mpsc::Receiver<PvField>> {
            None
        }
    }

    #[tokio::test]
    async fn pva_r14_source_calls_no_head_of_line_block() {
        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 100;
        let peer: SocketAddr = "127.0.0.1:9001".parse().unwrap();

        let source: DynSource = std::sync::Arc::new(SlowGetSource);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let cred = ClientCredentials::anonymous();

        let intro = FieldDesc::Variant;
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        let mut ops: HashMap<u32, OpState> = HashMap::new();
        ops.insert(
            ioid,
            non_monitor_op_state(intro.clone(), OpKind::Get, crate::proto::BitSet::new()),
        );
        channels.insert(
            sid,
            ChannelState {
                name: "slow".into(),
                cid: 1,
                sid,
                introspection: Some(intro),
                source: source.clone(),
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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
        async fn subscribe(&self, _: &str) -> Option<tokio::sync::mpsc::Receiver<PvField>> {
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
            non_monitor_op_state(intro.clone(), kind, BitSet::new()),
        );
        let mut channels = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 1,
                sid,
                introspection: Some(intro),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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

    fn destroy_frame(sid: u32, ioid: u32, order: ByteOrder) -> Frame {
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        synth_frame(Command::DestroyRequest, order, payload)
    }

    /// Poll `counter` until it reaches `want`, panicking after ~1 s. Used to
    /// wait for a spawned task to enter its (blocking) source call.
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
    #[tokio::test]
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
        let cred = ClientCredentials::anonymous();
        let mon = mpsc::unbounded_channel::<MonitorFinished>().0;
        let (exec_tx, _exec_rx) = mpsc::unbounded_channel::<ExecFinished>();

        // First EXEC: op was Idle → accepted, now Executing, source read spawned.
        handle_op(
            &get_exec_frame(sid, ioid, order),
            &tx,
            &mut channels,
            order,
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
    #[tokio::test]
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
        let cred = ClientCredentials::anonymous();
        let mon = mpsc::unbounded_channel::<MonitorFinished>().0;
        let (exec_tx, _exec_rx) = mpsc::unbounded_channel::<ExecFinished>();

        handle_op(
            &put_exec_frame(sid, ioid, order),
            &tx,
            &mut channels,
            order,
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
    #[tokio::test]
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
        let cred = ClientCredentials::anonymous();
        let mon = mpsc::unbounded_channel::<MonitorFinished>().0;
        let (exec_tx, _exec_rx) = mpsc::unbounded_channel::<ExecFinished>();

        handle_op(
            &get_exec_frame(sid, ioid, order),
            &tx,
            &mut channels,
            order,
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
    #[tokio::test]
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
        let cred = ClientCredentials::anonymous();
        let mon = mpsc::unbounded_channel::<MonitorFinished>().0;
        let (exec_tx, mut exec_rx) = mpsc::unbounded_channel::<ExecFinished>();

        // First EXEC completes; the task sends its response, then its
        // ExecFinishGuard drops and signals the owner.
        handle_op(
            &get_exec_frame(sid, ioid, order),
            &tx,
            &mut channels,
            order,
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
    #[tokio::test]
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

    /// Regression R0604-PVASRV-LASTREQUEST-ERROR-1. pvxs `ServerGPR::doReply`
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
        let mut op = non_monitor_op_state(FieldDesc::Variant, OpKind::Get, BitSet::new());
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
                introspection: Some(FieldDesc::Variant),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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
    /// op. The success-gating from R0604-PVASRV-LASTREQUEST-ERROR-1 must apply
    /// ONLY to GPR kinds; a naive `last_request && success` for all kinds would
    /// leak a failed introspection's reserved IOID. Lock the kind distinction:
    /// an error completion of a GET_FIELD op still frees it.
    #[test]
    fn get_field_one_shot_removed_on_error_completion() {
        let (sid, ioid) = (4u32, 90u32);
        let source: DynSource = Arc::new(crate::server_native::SharedSource::new());
        let mut op = non_monitor_op_state(
            FieldDesc::Variant,
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
                introspection: Some(FieldDesc::Variant),
                source,
                stat: crate::server_native::peers::ChannelStat::new(String::new()),
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
