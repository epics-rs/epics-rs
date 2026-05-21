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
    /// pvxs `MonitorOp::ackAt` (`servermon.cpp:68`) — the pipeline
    /// ACK-refill threshold parsed from `record._options.ackAny`. It
    /// caps the source-provided monitor watermarks at `ack_at - 1`
    /// (`servermon.cpp:332-333`, see [`clamp_watermarks`]). Defaults to
    /// `1` when `ackAny` is absent; only meaningful when `enabled`.
    ack_at: u32,
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
fn ack_at_from(ack_any: Option<&PvField>, queue_size: u32) -> u32 {
    use crate::pvdata::ScalarValue;
    // pvxs `MonitorOp::ackAt` struct default.
    let mut ack_at: u32 = 1;
    if let Some(PvField::Scalar(sv)) = ack_any {
        match sv {
            ScalarValue::String(s) => {
                if let Some(pct) = s.strip_suffix('%').filter(|p| !p.is_empty()) {
                    if let Ok(percent) = pct.trim().parse::<f64>() {
                        // servermon.cpp:564 multiplies the clamped percent
                        // by `limit` directly (no `/ 100`); the `[1, limit]`
                        // clamp below bounds the result. Replicated verbatim
                        // so the threshold matches a pvxs server.
                        ack_at = (percent.clamp(0.0, 100.0) * queue_size as f64) as u32;
                    }
                } else if let Ok(n) = s.trim().parse::<u32>() {
                    ack_at = n;
                }
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
        }
    }
    // servermon.cpp:577-581.
    if ack_at == 0 {
        ack_at = queue_size / 2;
    }
    ack_at.clamp(1, queue_size)
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
///   carried losslessly (EX-R12) so the `arr` transformation filter
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
/// [`apply_filter_transform`]; see EX-R12.
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
/// `None` for shapes with no representable value leaf (EX-R12: the
/// `arr` filter needs the real array, not a scalar fallback).
fn pv_value_leaf_to_epics(f: &PvField) -> Option<epics_base_rs::types::EpicsValue> {
    use crate::pvdata::ScalarValue;
    use epics_base_rs::types::EpicsValue;

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
                        _ => String::new(),
                    })
                    .collect(),
            ),
            // PF-R1: a PVA `ulong[]` monitor value must reach the
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

/// EX-R12: bridge a filter-chain-transformed `FilteredMonitorEvent`
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
    });
    // pvxs `servermon.cpp:554` — `record._options.ackAny`. Parsed only
    // for an enabled pipeline (`ackAt` is meaningless without one).
    let ack_any = opt_s
        .fields
        .iter()
        .find_map(|(k, v)| (k == "ackAny").then_some(v));
    let opts = if enabled {
        match queue_size {
            // Valid: use the requested window (pvxs `op->limit = qSize`).
            Some(n) if n >= 2 => PipelineOptions {
                enabled: true,
                queue_size: n,
                ack_at: ack_at_from(ack_any, n),
            },
            // PRESENT but invalid (`<2` or unparseable): pvxs
            // `servermon.cpp:537-540` rejects the INIT — the pipeline
            // sub-protocol requires agreement on `queueSize`. Do NOT
            // downgrade to a non-pipeline monitor.
            _ if queue_size_present => return Some(MonitorPipelineRequest::Reject),
            // ABSENT: pvxs keeps the default `limit` (4) and leaves
            // pipeline enabled.
            _ => PipelineOptions {
                enabled: true,
                queue_size: 4,
                ack_at: ack_at_from(ack_any, 4),
            },
        }
    } else {
        // Non-pipeline: a valid `queueSize` sets the queue depth
        // (pvxs sets `op->limit` regardless of pipeline); an invalid or
        // absent one keeps the default 4 (pvxs warns and ignores).
        let queue_size = match queue_size {
            Some(n) if n >= 2 => n,
            _ => 4,
        };
        PipelineOptions {
            enabled: false,
            queue_size,
            ack_at: 1,
        }
    };
    Some(MonitorPipelineRequest::Options(opts))
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
    /// PVA-FR-8: pulsed on RESUME so the subscriber loop wakes and
    /// flushes the value it squashed while paused — for both pipelined
    /// and non-pipelined monitors (the credit `monitor_window_notify`
    /// is `None` for non-pipelined, so resume needs its own wake). pvxs
    /// keeps posting into the monitor queue while Idle and drains it on
    /// START (`servermon.cpp:211-220,671-688`); the Rust equivalent is
    /// "hold the squashed latest value, emit on resume".
    monitor_resume: Arc<tokio::sync::Notify>,
    /// PVA-FR-4 / BRIDGE-FR-11: per-PV pipeline-window watermark levels
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
    /// BRIDGE-FR-11: HIGH MUST fire from the ACK path. A gateway source
    /// pauses its single upstream monitor on LOW; while paused no further
    /// events arrive, so firing HIGH from the event loop (the pre-FR-11
    /// behaviour) could never re-fire — the upstream would stay paused
    /// forever. pvxs fires `onHighMark` from the ACK handler
    /// (`servermon.cpp:653-666`).
    ///
    /// BRIDGE-FR-11 review: the value is also threaded to the source's
    /// `notify_watermark` as an ordering token so a gateway applying
    /// pause/resume out of process can discard a re-ordered command —
    /// closing a residual race where a resume could be lost behind a
    /// stale pause across the two firing tasks.
    monitor_wm_seq: Arc<std::sync::atomic::AtomicU64>,
    /// BRIDGE-FR-11 review (round 3): process-unique id for THIS monitor
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
    /// BR-R3: full INIT pvRequest value (decoded). PVA PUT INIT
    /// carries per-operation options (`record._options.process` /
    /// `block`, etc.) that the data-phase payload does NOT carry.
    /// We stash the value here at INIT so the data-phase PUT can
    /// attach it to the [`ChannelContext`] forwarded to the source,
    /// letting sources like the QSRV bridge honor process/block
    /// without re-parsing the value (where they no longer live).
    pv_request: Option<PvField>,
    /// BR-R14: event-affecting MONITOR pvRequest options
    /// (`pipeline` / `queueSize` / `_filter`) decoded at INIT. Passed
    /// to the source's `subscribe_*_checked_opts` at START so a
    /// fanout source (PVA gateway) can reject options it cannot honor
    /// transparently across a shared upstream monitor.
    monitor_options: crate::server_native::source::MonitorOptions,
    /// PVA-R14: abort guard for the spawned data-phase task (GET /
    /// PUT / RPC / PUT_GET / PROCESS exec). When a DESTROY_REQUEST
    /// arrives, dropping the Op removes this Arc; once the last clone
    /// is dropped, `AbortOnDrop::drop()` fires and the task is
    /// cancelled, preventing a stale response from reaching the
    /// client after DESTROY. Idle (INIT-only) and MONITOR ops leave
    /// this as `None`.
    data_task_abort: Option<Arc<AbortOnDrop>>,
    /// PVA-FR-11: single owner of this MONITOR op's Executing<->Idle edge
    /// (see [`MonitorStartControl`]). `Some` once the subscriber task is
    /// spawned; `None` for GET/PUT/RPC ops and for a MONITOR op that has
    /// been INIT'd but never STARTed. Dropping the `OpState` (DESTROY /
    /// channel destroy / connection reset) drops this Arc; the last drop
    /// fires the terminal `notify_monitor_start(false)` iff still
    /// executing.
    monitor_start_ctl: Option<Arc<MonitorStartControl>>,
}

/// BRIDGE-FR-11: atomically cross a pipeline-window watermark and mint
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
    /// frame, so it must not consume a slot — EX-R1).
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
        // PVA-FR-4: LOW fires when consuming this credit drained the
        // window to `<= low` (pvxs `onLowMark`). BRIDGE-FR-11:
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

/// BRIDGE-FR-11 review: finalizer that withdraws this monitor op's
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

/// PVA-FR-11: single owner of one MONITOR op's Executing<->Idle edge.
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
    ) -> Self {
        Self {
            src,
            pv_name,
            ctx,
            executing: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Edge-triggered: fire `notify_monitor_start(desired)` only when the
    /// executing state actually changes to `desired`.
    fn set(&self, desired: bool) {
        if self
            .executing
            .swap(desired, std::sync::atomic::Ordering::Relaxed)
            != desired
        {
            self.src
                .notify_monitor_start(&self.pv_name, &self.ctx, desired);
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
/// [`PeerRegistry`](crate::server_native::PeerRegistry). F-G7: lets [`crate::server_native::PvaServer::report`]
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

                    // F-G7: register this connection in the peer registry
                    // so PvaServer::report() can surface it. Deferred to
                    // here (post-peek) so the `tls` flag reflects the
                    // actual protocol, not the server config.
                    let peer_entry = crate::server_native::peers::PeerEntry::new(is_tls_client);
                    peers_for_task.insert(peer, peer_entry.clone());

                    let result = match (acceptor, is_tls_client) {
                        // Round 8 P-G15: cap the TLS handshake — a peer
                        // that completes TCP but stalls during ClientHello
                        // would otherwise hold a `max_connections` slot
                        // until OS keepalive reaps it (~30s).
                        (Some(a), true) => {
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

/// PVA-R14: result of a spawned CREATE_CHANNEL resolver task. The read
/// loop's `channels` HashMap is owned by the loop task; spawned
/// resolver tasks cannot touch it directly. Instead they send this
/// completion record through a dedicated mpsc, and the read loop's
/// `select!` arm applies the insertion and emits the wire response in
/// frame-arrival order (mpsc is FIFO).
struct CreateChannelCompletion {
    cid: u32,
    sid: u32,
    name: String,
    intro: Option<FieldDesc>,
    /// false → PV was not found; emit error response, no channel inserted.
    found: bool,
}
/// Sender half of the CREATE_CHANNEL completion channel.
type CcTx = mpsc::Sender<CreateChannelCompletion>;

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
    // PVA-R14: CREATE_CHANNEL completion channel. Spawned resolver
    // tasks send results here; the read loop's select! arm applies
    // insertions into `channels` and emits wire responses in arrival
    // order (mpsc FIFO preserves the per-frame ordering guarantee).
    let (cc_tx, mut cc_rx) = mpsc::channel::<CreateChannelCompletion>(64);
    // Count of in-flight CREATE_CHANNEL resolver tasks. Used in the
    // per-connection channel cap check: channels being resolved count
    // against the limit to prevent a burst of concurrent requests from
    // racing past it before the first completions arrive.
    let mut pending_channel_spawns: usize = 0;
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
        // PVA-R14: select! between CREATE_CHANNEL completions (from
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
                    if cc.found {
                        payload.put_u32(cc.sid, order);
                        Status::ok().write_into(order, &mut payload);
                        channels.insert(cc.sid, ChannelState {
                            name: cc.name,
                            cid: cc.cid,
                            sid: cc.sid,
                            introspection: cc.intro,
                            ops: HashMap::new(),
                        });
                        peer_entry.channel_added();
                        // PVA-FR-2: mirror live channel names for the report.
                        peer_entry
                            .set_channel_names(channels.values().map(|c| c.name.clone()).collect());
                    } else {
                        payload.put_u32(0u32, order);
                        Status::error(format!("unknown PV: {}", cc.name))
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
                    let _ = tx.send(buf).await;
                }
                continue;
            }
            frame_result = read_frame(&mut reader, &mut rx_buf, op_timeout, max_msg_size) => {
                frame_result?
            }
        };
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
                    // EX-R7: the client picked an auth method the
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
                    cred = ClientCredentials::anonymous();
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
                // PVA-FR-2: record the validated credentials for the
                // per-peer report.
                peer_entry.set_credentials(&cred.account, &cred.method);
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
                // PVA-R14: spawning version. Resolver tasks run
                // has_pv() + get_introspection() in the background;
                // results arrive via cc_rx and are applied at the top
                // of the loop. channel_added() is called there, so we
                // do not track it here.
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
                let before = channels.len();
                handle_destroy_channel(&frame, &tx, &mut channels, order).await?;
                if channels.len() < before {
                    peer_entry.channel_removed();
                    // PVA-FR-2: keep the report's channel-name mirror current.
                    peer_entry
                        .set_channel_names(channels.values().map(|c| c.name.clone()).collect());
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
                handle_get_field(&source, &frame, &tx, &channels, order, peer, &cred).await?;
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

/// Decode the VALUE body of an INIT pvRequest, after its descriptor
/// has already been read from `cur`.
///
/// Distinguishes an ABSENT value body (the cursor is exhausted after
/// the descriptor — the Rust client's RPC INIT sends only the type)
/// from a PRESENT but malformed one. pvxs `from_wire_type_value`
/// requires type+value and resets the connection on `!M.good()`
/// (`serverget.cpp:368-371`, `servermon.cpp:489`). We tolerate the
/// absent body for Rust↔Rust interop, but a present-but-undecodable
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
) -> Result<Option<PvField>, String> {
    if cur.position() as usize >= cur.get_ref().len() {
        return Ok(None);
    }
    decode_pv_field(req_desc, cur, order)
        .map(Some)
        .map_err(|e| format!("invalid pvRequest value: {e}"))
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
        let req_value = match decode_init_pv_request_value(&mut cur, &req_desc, order) {
            Ok(v) => v,
            Err(e) => {
                send_op_error(tx, OpKind::PutGet, ioid, &e, order).await?;
                return Ok(());
            }
        };
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

        // MR-R10: stash the INIT pvRequest so the data phase can
        // forward `record._options` (process/block, group `atomic`)
        // through `ChannelContext.pv_request` to the source. The
        // dedicated PUT_GET path otherwise dropped it, so QSRV group
        // PUT_GET could not honor INIT options on the native wire.
        let mut put_get_op = non_monitor_op_state(intro.clone(), OpKind::PutGet, mask);
        put_get_op.pv_request = req_value;
        ch.ops.insert(ioid, put_get_op);

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
    let (intro, mask, init_pv_request) = match op {
        Some(o) => {
            // EX-R5: the data-phase command must match the operation
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
            send_op_error(tx, OpKind::PutGet, ioid, "operation not initialised", order).await?;
            return Ok(());
        }
    };
    let pv_name = ch.name.clone();

    // Decode the put payload inline (cursor is borrowed from the stack frame).
    let changed = BitSet::decode(&mut cur, order).map_err(|e| PvaError::Decode(e.to_string()))?;
    let put_delta = decode_pv_field_with_bitset(&intro, &changed, 0, &mut cur, order)
        .map_err(|e| PvaError::Decode(format!("PUT_GET requires a value payload: {e}")))?;

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
    let tx_clone = tx.clone();
    let join = tokio::spawn(async move {
        let mut payload = Vec::new();
        payload.put_u32(ioid, order);
        payload.put_u8(subcmd);

        // PUT leg — WRITE-gated.
        let put_result = {
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
            src.put_delta_checked(checked, intro.clone(), changed, put_delta, ctx.clone())
                .await
        };
        if let Err(msg) = put_result {
            Status::error(msg).write_into(order, &mut payload);
            let h =
                PvaHeader::application(true, order, Command::PutGet.code(), payload.len() as u32);
            let mut buf = Vec::new();
            h.write_into(&mut buf);
            buf.extend_from_slice(&payload);
            let _ = tx_clone.send(buf).await;
            return;
        }

        // GET leg — READ-gated.
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
        match src.get_value_checked(read_checked, ctx).await {
            Some(v) => {
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
            None => {
                Status::ok().write_into(order, &mut payload);
                let empty = BitSet::with_capacity(intro.total_bits());
                empty.write_into(order, &mut payload);
            }
        }
        let h = PvaHeader::application(true, order, Command::PutGet.code(), payload.len() as u32);
        let mut buf = Vec::new();
        h.write_into(&mut buf);
        buf.extend_from_slice(&payload);
        let _ = tx_clone.send(buf).await;
    });
    if let Some(op_mut) = ch.ops.get_mut(&ioid) {
        op_mut.data_task_abort = Some(Arc::new(AbortOnDrop(join.abort_handle())));
    }
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
    match ch.ops.get(&ioid) {
        None => {
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
        Some(o) => {
            // EX-R5: the data-phase command must match the operation
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
        }
    }
    let pv_name = ch.name.clone();
    let ctx = crate::server_native::source::ChannelContext {
        peer,
        account: cred.account.clone(),
        method: cred.method.clone(),
        host: cred.host.clone(),
        authority: cred.authority.clone(),
        roles: cred.roles.clone(),
        pv_request: None,
    };
    let src = source.clone();
    let tx_clone = tx.clone();
    let join = tokio::spawn(async move {
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
        let result = src.process_checked(checked, ctx).await;

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
    if let Some(op_mut) = ch.ops.get_mut(&ioid) {
        op_mut.data_task_abort = Some(Arc::new(AbortOnDrop(join.abort_handle())));
    }
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

/// PVA-R14: spawn-based CREATE_CHANNEL handler. For each (cid, name)
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
    let mut cur = frame.cursor();
    // pvxs `serverchan.cpp:269-358`: a single CREATE_CHANNEL frame
    // can carry `count` (cid, name) pairs and the server must emit
    // one CREATE_CHANNEL response frame per pair, in arrival order.
    let count = cur
        .get_u16(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;

    // Collect entries to resolve asynchronously. We allocate SIDs
    // up-front so the cap is known before spawning, then spawn ONE
    // task that resolves names sequentially — this guarantees responses
    // arrive in arrival order (pvxs serverchan.cpp parity).
    let mut batch: Vec<(u32, u32, String)> = Vec::new(); // (cid, sid, name)

    for _ in 0..count {
        // PVA-R28: truncated CID / malformed string is a protocol-
        // fatal decode error. pvxs `serverchan.cpp:364-368`.
        let cid = cur
            .get_u32(order)
            .map_err(|e| PvaError::Decode(format!("CREATE_CHANNEL cid: {e}")))?;
        let name = match crate::proto::decode_string(&mut cur, order)
            .map_err(|e| PvaError::Decode(format!("CREATE_CHANNEL name: {e}")))?
        {
            Some(s) => s,
            None => break,
        };
        if name.is_empty() {
            break;
        }

        // A-G1 per-channel cap check: open channels + in-flight spawns
        // from previous frames + already-batched names in this frame.
        if channels.len() + *pending_channel_spawns + batch.len() >= max_channels_per_connection {
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
        // BRIDGE-FR-8: resolve existence + introspection under the
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
                let found = src.has_pv_checked(&nm, conn_ctx.clone()).await;
                let intro = if found {
                    src.get_introspection_checked(&nm, conn_ctx.clone()).await
                } else {
                    None
                };
                let _ = cc
                    .send(CreateChannelCompletion {
                        cid,
                        sid,
                        name: nm,
                        intro,
                        found,
                    })
                    .await;
            }
        });
    }
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
            // PVA-FR-11: CANCEL_REQUEST is Executing->Idle. Route through
            // the op's single start-control owner so notify_monitor_start(
            // false) fires once on the edge (no-op if already paused or
            // never started). DESTROY's terminal stop comes from Drop.
            if let Some(ctl) = &op.monitor_start_ctl {
                ctl.set(false);
            }
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
        // `send_op_error` above. For the VALUE body, distinguish an
        // ABSENT body (the Rust client's RPC INIT sends only the
        // descriptor — tolerated for interop) from a PRESENT but
        // malformed one. pvxs `from_wire_type_value` + `!M.good()`
        // resets on either; we tolerate absence but reject a
        // present-but-undecodable value, so a malformed pvRequest can no
        // longer silently drop `_filter` / pipeline / `process`|`block`
        // options behind an OK INIT. See `decode_init_pv_request_value`.
        let req_value = match decode_init_pv_request_value(&mut cur, &req_desc, order) {
            Ok(v) => v,
            Err(e) => {
                send_op_error(tx, kind, ioid, &e, order).await?;
                return Ok(());
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
        let pipeline_req = req_value.as_ref().and_then(monitor_pipeline_options);
        // pvxs `servermon.cpp:537-540`: a MONITOR pipeline request whose
        // PRESENT `queueSize` is invalid (`<2`/unparseable) is a
        // negotiation error — reject the INIT (`ctrl->error(...)` +
        // `return`) instead of silently downgrading to a non-pipeline
        // monitor. GET/PUT/RPC never negotiate pipeline (pvxs
        // `serverget` ignores these options), so the reject is
        // monitor-only.
        if kind == OpKind::Monitor && matches!(pipeline_req, Some(MonitorPipelineRequest::Reject)) {
            send_op_error(
                tx,
                kind,
                ioid,
                "can not pipeline invalid queueSize (must be >= 2)",
                order,
            )
            .await?;
            return Ok(());
        }
        let pipeline_opt = match pipeline_req {
            Some(MonitorPipelineRequest::Options(o)) => Some(o),
            _ => None,
        }
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
            && let Some(opt) = pipeline_opt.as_ref()
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

        // BR-R14: capture the event-affecting MONITOR pvRequest
        // options so the START path can hand them to the source's
        // `subscribe_*_checked_opts`. `pipeline_opt` was already
        // filtered to `enabled`; `queue_size` is recorded only when
        // the client requested pipeline mode (pvxs `servermon.cpp:533`
        // only honours `queueSize` for pipeline subscriptions).
        // `server_filter` reflects whether a non-empty `_filter`
        // chain was present.
        let monitor_options = if kind == OpKind::Monitor {
            crate::server_native::source::MonitorOptions {
                pipeline: pipeline_opt.is_some(),
                queue_size: pipeline_opt.as_ref().map(|o| o.queue_size),
                server_filter: !monitor_filters.is_empty(),
            }
        } else {
            crate::server_native::source::MonitorOptions::default()
        };

        // PVA-FR-4 / BRIDGE-FR-11: capture the source's pipeline-window
        // watermark levels at INIT so the subscriber loop (LOW) and the
        // ACK dispatch (HIGH) evaluate the same `(low, high)` against the
        // shared hysteresis flag. pvxs `servermon.cpp:332-333`: the
        // pipeline `ackAny`/`ackAt` threshold caps those levels at
        // `ackAt - 1`. Clamping here, once, is what makes both crossings
        // honor `ackAny` identically (the subscriber loop reads the
        // value threaded out of this `OpState`, not a fresh source read).
        let monitor_wm = if kind == OpKind::Monitor {
            clamp_watermarks(
                source.monitor_watermarks(&ch.name),
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
            // PVA-R14: spawn the data-phase work so the read loop can
            // continue parsing frames while the source future runs.
            let pv_name = ch.name.clone();
            let src = source.clone();
            let tx_clone = tx.clone();
            let intro_t = intro.clone();
            let mask_t = mask.clone();
            let cred_account = cred.account.clone();
            let cred_method = cred.method.clone();
            let cred_host = cred.host.clone();
            let cred_authority = cred.authority.clone();
            let cred_roles = cred.roles.clone();
            // MR-R13: forward the decoded INIT pvRequest into the GET
            // context so QSRV group GET honors `record._options`
            // (e.g. `atomic`). Previously dropped here as `None`.
            let init_pv_request_t = init_pv_request.clone();
            // Abort any previous in-flight data task for this ioid
            // (e.g. double-EXEC from a misbehaving client).
            if let Some(op_mut) = ch.ops.get_mut(&ioid) {
                op_mut.data_task_abort = None;
            }
            let join = tokio::spawn(async move {
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
                let value = match src.get_value_checked(checked, ctx).await {
                    Some(v) => v,
                    None => {
                        let _ = send_op_error(&tx_clone, OpKind::Get, ioid, "PV not found", order)
                            .await;
                        return;
                    }
                };
                // PVA-R9: source-side mismatch gate.
                if let Err(e) = crate::pvdata::value_matches_descriptor(&value, &intro_t) {
                    let _ = send_op_error(
                        &tx_clone,
                        OpKind::Get,
                        ioid,
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
                let _ = tx_clone.send(buf).await;
            });
            if let Some(op_mut) = ch.ops.get_mut(&ioid) {
                op_mut.data_task_abort = Some(Arc::new(AbortOnDrop(join.abort_handle())));
            }
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
                // PVA-R14: spawn GET-for-PUT-readback — blocks on
                // source.get_value_checked which can be slow.
                let pv_name = ch.name.clone();
                let src = source.clone();
                let tx_clone = tx.clone();
                let intro_t = intro.clone();
                let mask_t = mask.clone();
                let cred_account = cred.account.clone();
                let cred_method = cred.method.clone();
                let cred_host = cred.host.clone();
                let cred_authority = cred.authority.clone();
                let cred_roles = cred.roles.clone();
                // MR-R13: forward the INIT pvRequest into the PUT
                // readback GET context so the readback honors the
                // same `record._options` the GET path would.
                let init_pv_request_t = init_pv_request.clone();
                if let Some(op_mut) = ch.ops.get_mut(&ioid) {
                    op_mut.data_task_abort = None;
                }
                let join = tokio::spawn(async move {
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
                    let value = match src.get_value_checked(checked, ctx).await {
                        Some(v) => v,
                        None => {
                            let _ =
                                send_op_error(&tx_clone, OpKind::Put, ioid, "PV not found", order)
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
                    let _ = tx_clone.send(buf).await;
                });
                if let Some(op_mut) = ch.ops.get_mut(&ioid) {
                    op_mut.data_task_abort = Some(Arc::new(AbortOnDrop(join.abort_handle())));
                }
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
            // PVA-R14: spawn PUT exec — put_delta_checked can be slow.
            // Decode frame data synchronously (above) so the cursor is
            // consumed before returning; source calls happen in the task.
            let src = source.clone();
            let tx_clone = tx.clone();
            let intro_t = intro.clone();
            let cred_account = cred.account.clone();
            let cred_method = cred.method.clone();
            let cred_host = cred.host.clone();
            let cred_authority = cred.authority.clone();
            let cred_roles = cred.roles.clone();
            let init_pv_request_t = init_pv_request.clone();
            if let Some(op_mut) = ch.ops.get_mut(&ioid) {
                op_mut.data_task_abort = None;
            }
            let join = tokio::spawn(async move {
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
                    src.put_delta_checked(
                        checked,
                        intro_t.clone(),
                        changed.clone(),
                        delta,
                        ctx.clone(),
                    )
                    .await
                };
                let mut payload = Vec::new();
                payload.put_u32(ioid, order);
                payload.put_u8(subcmd);
                match result {
                    Ok(()) => {
                        if subcmd & 0x40 != 0 {
                            // PUT_GET readback (R31-G7): build readback
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
                            match src.get_value_checked(read_checked, ctx).await {
                                Some(v) => {
                                    Status::ok().write_into(order, &mut payload);
                                    let bits = BitSet::all_set(intro_t.total_bits());
                                    bits.write_into(order, &mut payload);
                                    encode_pv_field(&v, &intro_t, order, &mut payload);
                                }
                                None => {
                                    Status::ok().write_into(order, &mut payload);
                                    let empty = BitSet::with_capacity(intro_t.total_bits());
                                    empty.write_into(order, &mut payload);
                                }
                            }
                        } else {
                            Status::ok().write_into(order, &mut payload);
                        }
                    }
                    Err(msg) => Status::error(msg).write_into(order, &mut payload),
                }
                let h =
                    PvaHeader::application(true, order, Command::Put.code(), payload.len() as u32);
                let mut buf = Vec::new();
                h.write_into(&mut buf);
                buf.extend_from_slice(&payload);
                let _ = tx_clone.send(buf).await;
            });
            if let Some(op_mut) = ch.ops.get_mut(&ioid) {
                op_mut.data_task_abort = Some(Arc::new(AbortOnDrop(join.abort_handle())));
            }
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
                    // PVA-FR-11: Executing->Idle. The op's single
                    // start-control owner fires notify_monitor_start(false)
                    // on the edge so a gateway can suspend its upstream.
                    if let Some(ctl) = &op.monitor_start_ctl {
                        ctl.set(false);
                    }
                } else if is_resume {
                    let prev = op
                        .monitor_paused
                        .swap(false, std::sync::atomic::Ordering::Relaxed);
                    if prev {
                        if let Some(n) = op.monitor_window_notify.as_ref() {
                            n.notify_waiters();
                        }
                        // PVA-FR-8: wake the subscriber loop so it flushes
                        // the value squashed during the pause (works for
                        // non-pipelined ops too, which have no window
                        // notify). `notify_one` stores a permit when the
                        // loop isn't waiting yet, so a resume that races
                        // ahead of the loop's `notified()` is not lost.
                        op.monitor_resume.notify_one();
                        // PVA-FR-11: Idle->Executing. `prev` gates this to a
                        // genuine resume — a START on a monitor that was not
                        // actually paused does not re-fire on_start(true).
                        if let Some(ctl) = &op.monitor_start_ctl {
                            ctl.set(true);
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
                // BRIDGE-FR-11: fire HIGH (resume) from the ACK path —
                // pvxs `servermon.cpp:653-666` fires `onHighMark` when
                // ACKs add enough credit. A gateway source that paused
                // its single upstream monitor on LOW receives no further
                // events while paused, so the event-loop HIGH check could
                // never re-fire; the resume MUST be driven by the credit
                // refill here. `fire_high` (the crossing's ordering
                // token) is computed under the `op` borrow, then the
                // callback runs after it is dropped so `source` can
                // borrow `ch.name` freely.
                // `(seq, op_id)` of the crossing, computed under the `op`
                // borrow, fired after it is dropped so `source` can borrow
                // `ch.name` freely.
                let mut fire_high: Option<(u64, u64)> = None;
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
                        // HIGH fires once per crossing: the refilled
                        // window stands above `high`. `cross_watermark`
                        // both checks-and-marks the crossing and mints the
                        // ordering token in one CAS, returning `Some(seq)`
                        // exactly on the below→above edge. The companion
                        // LOW (event loop) crosses back when a DATA
                        // emission drains to `<= low`.
                        if let Some((_lo, hi)) = op.monitor_wm {
                            let w_now = prev as usize + ack_count as usize;
                            if w_now > hi {
                                fire_high = cross_watermark(&op.monitor_wm_seq, true)
                                    .map(|seq| (seq, op.monitor_op_id));
                            }
                        }
                    }
                }
                if let Some((seq, op_id)) = fire_high {
                    // BRIDGE-FR-11 review: thread this connection's
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
                    roles: cred.roles.clone(),
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
                // Snapshot the window + notify so the spawned task can
                // share state with this dispatch path's ACK handler.
                // BR-R14: also lift the event-affecting monitor
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
                            // BRIDGE-FR-11: the LOW callback in the loop
                            // below crosses this shared counter back to
                            // "below"; the HIGH callback in the ACK dispatch
                            // crosses it to "above". Sharing it keeps the
                            // pause/resume hysteresis AND the monotonic
                            // ordering token coherent across the two paths.
                            s.monitor_wm_seq.clone(),
                            // BRIDGE-FR-11 review: same op identity the ACK
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
                let join = tokio::spawn(async move {
                    // PVA-R14: access gate check moved inside the spawn
                    // so the read loop is not blocked while the ACF
                    // policy resolves. The version capture must precede
                    // the check (see R49-G1 audit note above).
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
                            .subscribe_raw_checked_opts(
                                mon_checked.clone(),
                                mon_ctx.clone(),
                                monitor_options.clone(),
                            )
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
                        // PVA-FR-8: raw fast path honors the pause gate
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
                            // BR-R42: an upstream descriptor change
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
                            // PVA-FR-8: a pause that began after the
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
                            // BR-R44: on byte-order mismatch we must
                            // decode the raw event under the upstream
                            // order and re-encode under the downstream
                            // order. Earlier code dropped the event
                            // with `continue`, so any cross-host
                            // gateway between peers with different
                            // negotiated byte orders silently lost
                            // every monitor update after the initial
                            // snapshot (the decoded-fallback path
                            // never sees those events under raw
                            // subscription).
                            let payload = if ev.byte_order != order {
                                match reencode_raw_monitor(ioid, &intro_clone, &ev, order) {
                                    Ok(p) => p,
                                    Err(e) => {
                                        debug!(
                                            pv = %pv_name,
                                            error = %e,
                                            "F-G12 raw monitor reencode failed — dropping event"
                                        );
                                        continue;
                                    }
                                }
                            } else {
                                build_monitor_payload_raw(ioid, &ev, order)
                            };
                            if tx_clone.send(payload).await.is_err() {
                                return;
                            }
                        }
                        let finish = build_monitor_finish(ioid, order);
                        let _ = tx_clone.send(finish).await;
                        return;
                    }

                    let Some(mut rx) = src
                        .subscribe_checked_opts(
                            mon_checked.clone(),
                            mon_ctx.clone(),
                            monitor_options.clone(),
                        )
                        .await
                    else {
                        return;
                    };
                    // Diagnostic-only outbound-queue-depth crossing flag.
                    let mut queue_over_high = false;
                    // PVA-FR-4: per-PV pipeline-window watermark levels
                    // `(low, high)` in credit units. `None` when the source
                    // exposes no per-PV levels. BRIDGE-FR-11: the hysteresis
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
                    // BRIDGE-FR-11 review: arm the withdraw-on-teardown
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
                    // BR-R29: does this source emit *partial* monitor
                    // updates (QSRV group monitor with a self-trigger)?
                    // When it does, every event after the first carries
                    // a wire changed-bitset narrowed to the leaves that
                    // actually changed — derived by structurally
                    // diffing consecutive snapshots, matching pvxs's
                    // marked-leaf semantics (`servermon.cpp:174`
                    // `to_wire_valid(R, ent, &pvMask)`). `prev_value`
                    // holds the last emitted snapshot for that diff.
                    let emits_partial = src.monitor_emits_partial(&pv_name);
                    let mut prev_value: Option<PvField> = None;
                    // Emit initial snapshot via the ACF-aware path —
                    // a peer with NoAccess on the record's ASG sees
                    // nothing; legacy sources fall through to
                    // `get_value` via the trait default.
                    if let Some(initial) = src
                        .get_value_checked(mon_checked.clone(), mon_ctx.clone())
                        .await
                    {
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
                    // Back-pressure / squashing loop: drain available
                    // events between writes, keeping only the most recent
                    // value if more than `queue_depth` events stack up.
                    let mut squashing = false;
                    // PVA-FR-8: value squashed while paused, flushed on
                    // resume. `None` between batches.
                    let mut held: Option<crate::server_native::MonitorUpdate> = None;
                    loop {
                        // Acquire the next value to consider emitting
                        // through the shared pause owner: a value held
                        // from a pause takes priority once resumed; while
                        // paused we keep receiving and squash to the
                        // latest without emitting, waking on resume to
                        // flush it (pvxs queues posts while Idle and
                        // drains on START). The decoded path has no
                        // subscription-boundary event, so nothing is
                        // yielded early while paused (`|_| false`).
                        // BRIDGE-FR-12: events coalesced during a pause
                        // union their marked-leaf sets via
                        // `coalesce_monitor_update`.
                        let mut value = match next_monitor_event(
                            &mut rx,
                            &mut held,
                            &paused_flag,
                            &resume_notify,
                            |_| false,
                            coalesce_monitor_update,
                        )
                        .await
                        {
                            Some(v) => v,
                            None => break,
                        };
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
                        // Drain extras; keep the latest value but union
                        // the coalesced events' marked-leaf sets
                        // (BRIDGE-FR-12).
                        let mut squashed = 0usize;
                        loop {
                            match rx.try_recv() {
                                Ok(next) => {
                                    value = coalesce_monitor_update(value, next);
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
                        // PVA-FR-4: outbound-queue depth is a SERVER
                        // diagnostic only — it is no longer used to fire
                        // the SharedPV watermark callbacks. pvxs ties
                        // `onHighMark`/`onLowMark` to the pipeline flow-
                        // control window, which we now do at the credit
                        // gate below. Counter is max_capacity - capacity
                        // since mpsc doesn't expose len directly.
                        let pending = tx_clone.max_capacity() - tx_clone.capacity();
                        if pending >= high_watermark && !queue_over_high {
                            queue_over_high = true;
                            warn!(
                                pv = %pv_name,
                                pending,
                                high_watermark,
                                "monitor outbound queue crossed high watermark"
                            );
                        } else if pending == 0 && queue_over_high {
                            queue_over_high = false;
                            debug!(pv = %pv_name, "monitor outbound queue drained");
                        }
                        // EX-R1: pause and filter suppression MUST run
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
                        // PVA-FR-8: a pause that began while this value was
                        // already in hand must HOLD it (squash to latest),
                        // not drop it — resume flushes the held value. This
                        // mirrors pvxs keeping posts in the monitor queue
                        // while Idle and draining on START. Like the drop
                        // it once was, holding consumes no pipeline credit
                        // (no wire frame is produced).
                        if paused_flag.load(std::sync::atomic::Ordering::Relaxed) {
                            held = Some(value);
                            continue;
                        }
                        // BRIDGE-FR-12: past the pause gate the wire frame is
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
                        // EX-R12: a filter chain may TRANSFORM the
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
                        // P-G11: pipeline window check, through the same
                        // single owner the initial snapshot uses. When
                        // pipeline is active this waits for a free window
                        // slot, consumes one credit, and fires LOW on the
                        // above→below crossing; for a non-pipeline monitor
                        // it is a no-op (mpsc backpressure stays the only
                        // gate). It runs after the pause/filter gates above
                        // so credit is consumed only for events that will
                        // produce a DATA frame (EX-R1).
                        credit.acquire().await;
                        // BRIDGE-FR-12: an explicit marked-leaf set from the
                        // source (a QSRV group `+trigger` target graph) takes
                        // precedence over both server-derived bitsets — pvxs
                        // `groupsource.cpp:288` marks each trigger target
                        // assigned-not-changed, so a value-diff would wrongly
                        // drop targets whose value did not move. The encoder
                        // turns the declared paths into the wire bitset
                        // intersected with the request mask.
                        //
                        // BR-R29: otherwise, for a partial-emitting source,
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
                // PVA-FR-11: install the single-owner Executing<->Idle edge
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
        }
        OpKind::Rpc => {
            // RPC DATA request from client: `type(arg) + full_value(arg)`.
            // pvxs clientget.cpp:307-311 — `to_wire(R, type); to_wire_full(R, arg)`.
            // Decode the argument inline (before spawning) because the cursor
            // is borrowed from the frame which lives on the read-loop stack.
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
            let _ = intro;
            let src = source.clone();
            let tx_clone = tx.clone();
            let rpc_ctx_val = crate::server_native::source::ChannelContext {
                peer,
                account: cred.account.clone(),
                method: cred.method.clone(),
                host: cred.host.clone(),
                authority: cred.authority.clone(),
                roles: cred.roles.clone(),
                pv_request: None,
            };
            let join = tokio::spawn(async move {
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
                let result = src
                    .rpc_checked(rpc_checked, req_desc, req_value, rpc_ctx_val)
                    .await;

                let mut payload = Vec::new();
                payload.put_u32(ioid, order);
                // pvxs `serverget.cpp:83` echoes the request subcmd.
                payload.put_u8(subcmd);
                match result {
                    Ok((resp_desc, resp_value)) => {
                        Status::ok().write_into(order, &mut payload);
                        // Spawned task cannot hold &mut EncodeTypeCache; use inline
                        // encode_type_desc (no cache) for RPC responses.
                        encode_type_desc(&resp_desc, order, &mut payload);
                        encode_pv_field(&resp_value, &resp_desc, order, &mut payload);
                    }
                    Err(msg) => Status::error(msg).write_into(order, &mut payload),
                }
                let h =
                    PvaHeader::application(true, order, Command::Rpc.code(), payload.len() as u32);
                let mut buf = Vec::new();
                h.write_into(&mut buf);
                buf.extend_from_slice(&payload);
                let _ = tx_clone.send(buf).await;
            });
            if let Some(op_mut) = ch.ops.get_mut(&ioid) {
                op_mut.data_task_abort = Some(Arc::new(AbortOnDrop(join.abort_handle())));
            }
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
    peer: SocketAddr,
    cred: &ClientCredentials,
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
    match chan.introspection.clone() {
        Some(intro) => {
            // Fast path: introspection already cached on the channel; no source call needed.
            let mut payload = Vec::new();
            payload.put_u32(ioid, order);
            Status::ok().write_into(order, &mut payload);
            encode_type_desc(&intro, order, &mut payload);
            let h =
                PvaHeader::application(true, order, Command::GetField.code(), payload.len() as u32);
            let mut buf = Vec::new();
            h.write_into(&mut buf);
            buf.extend_from_slice(&payload);
            let _ = tx.send(buf).await;
        }
        None => {
            // Slow path: introspection not yet cached; fetch from source without blocking
            // the read loop.
            let pv_name = chan.name.clone();
            let src = source.clone();
            let tx_clone = tx.clone();
            // BRIDGE-FR-8: introspect under the downstream connection's
            // identity. pvxs builds the GET_FIELD ConnectOp with
            // `conn->cred` (`serverintrospect.cpp:66`); a gateway must
            // resolve the upstream type against THIS peer's credentials,
            // not the shared identity. `pv_request` is `None` —
            // GET_FIELD carries no per-op pvRequest.
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
                let intro = src
                    .get_introspection_checked(&pv_name, conn_ctx)
                    .await
                    .unwrap_or(FieldDesc::Variant);
                let mut payload = Vec::new();
                payload.put_u32(ioid, order);
                Status::ok().write_into(order, &mut payload);
                encode_type_desc(&intro, order, &mut payload);
                let h = PvaHeader::application(
                    true,
                    order,
                    Command::GetField.code(),
                    payload.len() as u32,
                );
                let mut buf = Vec::new();
                h.write_into(&mut buf);
                buf.extend_from_slice(&payload);
                let _ = tx_clone.send(buf).await;
            });
        }
    }
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

/// PVA-FR-8: single owner of the monitor pause/hold/squash transition.
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
    // BRIDGE-FR-12: combine an already-held event with a newer one when
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

/// BR-R29: build a MONITOR data frame whose wire changed-bitset is
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

/// BRIDGE-FR-12: build a MONITOR data frame whose wire changed-bitset
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

/// BRIDGE-FR-12: coalesce two cooked monitor updates when the server
/// squashes events under back-pressure or pause. The newer value wins;
/// the marked-leaf sets union so the emitted frame still marks every
/// field that changed across the coalesced burst. A `None` on either
/// side means "no explicit set — derive by diff", which over-marks
/// safely, so the union of `None` with anything stays `None`.
fn coalesce_monitor_update(
    older: crate::server_native::MonitorUpdate,
    newer: crate::server_native::MonitorUpdate,
) -> crate::server_native::MonitorUpdate {
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
    }
}

/// BR-R44: decode a raw MONITOR event captured under upstream
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
    // The overrun bitset is optional in some upstream variants;
    // tolerate truncation by defaulting to empty.
    let overrun = BitSet::decode(&mut cur, ev.byte_order).unwrap_or_else(|_| BitSet::new());

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

/// EX-R12: build a MONITOR error frame — subcmd `0x10` (finish) plus
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

    /// BRIDGE-FR-11 review: `cross_watermark` is the primitive that
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

    /// BRIDGE-FR-11 review: the withdraw-on-teardown finalizer
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
            async fn put_value(&self, _name: &str, _value: PvField) -> Result<(), String> {
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

    /// PF-R1: a PVA `ulong[]` monitor value must reach the `arr`
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

    /// BRIDGE-FR-12: coalescing two cooked updates under pause/squash.
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

    /// PVA-FR-8 owner: invariant boundaries of [`next_monitor_event`],
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
        assert_eq!(opts.queue_size, 4, "absent queueSize → default window 4");
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
                decode_init_pv_request_value(&mut cur, &desc, ByteOrder::Little),
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
                decode_init_pv_request_value(&mut cur, &desc, ByteOrder::Little),
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
                decode_init_pv_request_value(&mut cur, &desc, ByteOrder::Little).is_err(),
                "a present-but-truncated value body must error, not collapse to None",
            );
        }
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
    fn ack_at_percentage_matches_pvxs_formula() {
        // servermon.cpp:564 computes `clamp(percent,0,100) * limit`
        // (NO `/ 100`), then clamps to [1, limit]. A fractional percent
        // is the only way to land strictly below `limit`.
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(100)),
            PvField::Scalar(ScalarValue::String("0.5%".into())),
        );
        assert_eq!(
            parsed_opts(&req).ack_at,
            50,
            "0.5% of limit=100 → 0.5*100 = 50 (verbatim pvxs arithmetic)"
        );
        // Any percent >= 1 saturates: 50 * 16 = 800 → clamp to 16.
        let req = make_pipeline_request_ack(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(16)),
            PvField::Scalar(ScalarValue::String("50%".into())),
        );
        assert_eq!(parsed_opts(&req).ack_at, 16);
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
        pv.open(intro.clone(), three_field_value(0, 0, 0));
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
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous();

        // MONITOR INIT with pipeline=true, queueSize=2 (no nack byte —
        // window initialises to queueSize).
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
            &source,
            &init_frame,
            &tx,
            &mut channels,
            order,
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            peer,
            &cred,
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
            &source,
            &start_frame,
            &tx,
            &mut channels,
            order,
            OpKind::Monitor,
            &config,
            &mut encode_cache,
            peer,
            &cred,
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

    /// BR-R29 residual regression: `build_monitor_payload_partial`
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
            "member `b` (bit 2) unchanged — must NOT be marked (BR-R29 narrowing)"
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
        let resp = rx.recv().await.expect("PUT EXEC response emitted");
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

    /// EX-R3 regression: the default `ChannelSource::put_delta_checked`
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
            ) -> impl std::future::Future<Output = Result<(), String>> + Send {
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
            "EX-R3: default put_delta_checked must merge the delta over the \
             credentialed prior (10,20,30); got ({a},{b},{c})"
        );
        assert!(
            !used_ctxless.load(Ordering::SeqCst),
            "EX-R3: the prior-value read must NOT go through the ctx-less get_value"
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
    /// so an EX-R5 wrong-kind data frame can be driven against it.
    #[cfg(test)]
    fn primed_channels_with_kind(sid: u32, ioid: u32, kind: OpKind) -> HashMap<u32, ChannelState> {
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
                ops,
            },
        );
        channels
    }

    /// EX-R5 regression: the dedicated `handle_process` data phase must
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
        let mut channels = primed_channels_with_kind(sid, ioid, OpKind::Get);
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();

        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x00);
        let frame = synth_frame(Command::Process, order, payload);

        let res = handle_process(
            &source,
            &frame,
            &tx,
            &mut channels,
            order,
            &config,
            peer,
            &x509_cred("MyCA"),
        )
        .await;
        assert!(
            res.is_err(),
            "EX-R5: PROCESS data against a GET-initialised IOID must be a protocol error"
        );
        assert_eq!(
            process_hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "EX-R5: record processing must NOT run for a wrong-kind PROCESS frame"
        );
    }

    /// EX-R5 regression: `handle_process` must also reject a PROCESS
    /// data frame against a MONITOR-initialised IOID.
    #[tokio::test]
    async fn ex_r5_process_data_rejects_monitor_initialised_ioid() {
        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 601;
        let src = AuthorityGatedSource::new();
        let process_hits = std::sync::Arc::clone(&src.process_hits);
        let source: DynSource = std::sync::Arc::new(src);

        let mut channels = primed_channels_with_kind(sid, ioid, OpKind::Monitor);
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();

        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x00);
        let frame = synth_frame(Command::Process, order, payload);

        let res = handle_process(
            &source,
            &frame,
            &tx,
            &mut channels,
            order,
            &config,
            peer,
            &x509_cred("MyCA"),
        )
        .await;
        assert!(
            res.is_err(),
            "EX-R5: PROCESS data against a MONITOR-initialised IOID must be a protocol error"
        );
        assert_eq!(
            process_hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "EX-R5: record processing must NOT run for a wrong-kind PROCESS frame"
        );
    }

    /// EX-R5 regression: the dedicated `handle_put_get` data phase must
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

        let mut channels = primed_channels_with_kind(sid, ioid, OpKind::Get);
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
        .await;
        assert!(
            res.is_err(),
            "EX-R5: PUT_GET data against a GET-initialised IOID must be a protocol error"
        );
    }

    /// EX-R5 regression: `handle_put_get` must also reject a PUT_GET
    /// data frame against a PUT-initialised IOID.
    #[tokio::test]
    async fn ex_r5_put_get_data_rejects_put_initialised_ioid() {
        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 603;
        let source: DynSource = std::sync::Arc::new(AuthorityGatedSource::new());

        let mut channels = primed_channels_with_kind(sid, ioid, OpKind::Put);
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
        .await;
        assert!(
            res.is_err(),
            "EX-R5: PUT_GET data against a PUT-initialised IOID must be a protocol error"
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

        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous();
        handle_get_field(&source, &frame, &tx, &channels, order, peer, &cred)
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

        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous();
        handle_get_field(&source, &frame, &tx, &channels, order, peer, &cred)
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

#[cfg(test)]
mod r14_tests {
    //! PVA-R14 regression: source calls must not block the TCP read loop.
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
        async fn put_value(&self, _name: &str, _value: PvField) -> Result<(), String> {
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
        handle_op(
            &source,
            &frame,
            &tx,
            &mut channels,
            order,
            OpKind::Get,
            &config,
            &mut encode_cache,
            peer,
            &cred,
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
