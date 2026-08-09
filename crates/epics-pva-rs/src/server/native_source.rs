//! [`ChannelSource`] implementation backed by an epics-rs [`PvDatabase`].
//!
//! Builds NTScalar and NTScalarArray `PvField` values directly from
//! `Snapshot`s, with full alarm/timeStamp/display metadata.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::nt::NTScalar;
use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue, TypedScalarArray};
use crate::server_native::source::SourceRead;
use crate::server_native::{ChannelSource, OpError};

use crate::server_native::source::{MonitorStream, UpstreamMonitor};
use epics_base_rs::server::database::{PvDatabase, PvEntry, parse_pv_name};
use epics_base_rs::server::recgbl::{alarm_condition_string, alarm_status};
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::{EpicsValue, WallTime};

/// Shared, mutable ACF cell — an alias for the base type, so the PVA server,
/// the CA server and the gateway all share one cell type. Lock-free:
/// `PvaServer::reload_acf_from` (and the `/reload-acf` introspection endpoint
/// behind it) publishes a new policy, and every `PvDatabaseSource` ACF check
/// takes an `Arc` snapshot rather than a read guard. See
/// `doc/rtems-priority-locks-design.md` §3 row L9.
pub type AcfCell = epics_base_rs::server::access_security::AcfCell;

/// Native `ChannelSource` over a `PvDatabase`.
pub struct PvDatabaseSource {
    db: Arc<PvDatabase>,
    /// The type-state access gate is the only ACF
    /// surface on this source. The gate holds an `Arc` clone of
    /// the caller's `AcfCell` (so hot-swap via that cell is
    /// visible) plus a per-name `(ASG, ASL)` resolver that reads
    /// the live record's `common.asg` / `common.asl`. Wire-layer
    /// `*_checked` ops cannot run without an `AccessChecked` minted
    /// here.
    gate: epics_base_rs::server::access_security::AccessGate,
}

impl PvDatabaseSource {
    pub fn new(db: Arc<PvDatabase>) -> Self {
        let acf: AcfCell = epics_base_rs::server::access_security::new_acf_cell(None);
        let gate = Self::build_gate(db.clone(), acf, None);
        Self { db, gate }
    }

    /// Build with ACF enforcement. Mirrors what `PvaServer::run`
    /// installs from its builder-supplied ACF — every PUT goes
    /// through `check_access_method` against the record's `ASG`.
    pub fn new_with_acf(db: Arc<PvDatabase>, acf: AcfCell) -> Self {
        let gate = Self::build_gate(db.clone(), acf, None);
        Self { db, gate }
    }

    /// Build with an externally-supplied
    /// `acl_version` counter. `PvaServer` shares the same `Arc` so
    /// its `reload_acf_from` / `clear_acf` can bump the version
    /// that this source's gate exposes, forcing monitor tasks
    /// spawned on top of this source to re-check on their next
    /// event.
    pub fn new_with_acf_and_version(
        db: Arc<PvDatabase>,
        acf: AcfCell,
        acl_version: Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        let gate = Self::build_gate(db.clone(), acf, Some(acl_version));
        Self { db, gate }
    }

    fn build_gate(
        db: Arc<PvDatabase>,
        acf: AcfCell,
        acl_version: Option<Arc<std::sync::atomic::AtomicU64>>,
    ) -> epics_base_rs::server::access_security::AccessGate {
        use epics_base_rs::server::access_security::{AccessGate, AsgAslResolver, InpResolver};
        let asg_db = db.clone();
        let resolver: AsgAslResolver = Arc::new(move |pv_name| {
            let db = asg_db.clone();
            Box::pin(async move {
                let (base, _field) = parse_pv_name(&pv_name);
                if let Some(rec) = db.get_record(base) {
                    let inst = rec.read();
                    return (inst.common.access_group().to_string(), inst.common.asl);
                }
                ("DEFAULT".to_string(), 0u8)
            })
        });
        // resolve an ACF `INP*` link (`record.field`) to its
        // current numeric value from the live database, so CALC-gated
        // rules evaluate against real values instead of failing closed.
        let inp_db = db.clone();
        let inp_resolver: InpResolver = Arc::new(move |link: String| {
            let db = inp_db.clone();
            Box::pin(async move {
                let (base, field) = parse_pv_name(&link);
                let field = if field.is_empty() { "VAL" } else { field };
                let rec = db.get_record(base)?;
                let inst = rec.read();
                inst.resolve_field(field).and_then(|v| v.to_f64())
            })
        });
        let gate = match acl_version {
            Some(v) => AccessGate::required_with_version(acf, resolver, v),
            None => AccessGate::required(acf, resolver),
        };
        gate.with_inp_resolver(inp_resolver)
    }

    pub fn database(&self) -> &Arc<PvDatabase> {
        &self.db
    }

    // the ASG/ASL resolution moved into the AccessGate
    // builder (`Self::build_gate`). The duplicate `resolve_asg`
    // method that the deleted `*_ctx` overrides used is now gone.
}

// ── EpicsValue → PvField (NTScalar / NTScalarArray) ─────────────────────

/// The NT shape QSRV declares for a non-`DBR_ENUM` channel.
///
/// pvxs builds every such prototype with
/// `nt::NTScalar{valueType, display=true, control=true, valueAlarm=true,
/// form=true}` (`ioc/singlesource.cpp:196-203`) — all four flags on,
/// unconditionally. [`crate::nt::NTScalar`] is this crate's port of
/// `NTScalar::build()` (`src/nt.cpp:37-114`) and therefore the single owner
/// of what that expands to: the `isnumeric` gate that drops `control` /
/// `valueAlarm` / the display limits for a string value, the limits typed
/// with `value.scalarOf()` rather than a hard-coded double, `display.form`,
/// and the Float64 `hysteresis`.
///
/// This function exists so the served value and the advertised type are
/// projections of ONE configuration. The shape was previously hand-rolled
/// here, in parallel with that builder, and had drifted from it on four
/// counts at once (no `display.form`, `uint8_t` `hysteresis`, `double`
/// limits on integer records, and a full numeric `display`/`control`/
/// `valueAlarm` on string records).
fn nt_scalar_for(snap: &Snapshot) -> NTScalar {
    // Same single owner as the value leaf (`crate::leaf_convert`), so the
    // NT's `value` member and the type its limits are cut from agree by
    // construction.
    let (value_type, is_array) =
        match crate::leaf_convert::epics_value_to_field_desc_leaf(&snap.value) {
            FieldDesc::Scalar(t) => (t, false),
            FieldDesc::ScalarArray(t) => (t, true),
            // `epics_value_to_field_desc_leaf` returns only those two variants
            // for an `EpicsValue`; a string scalar is the conservative shape if
            // that ever changes.
            _ => (ScalarType::String, false),
        };
    let mut nt = if is_array {
        NTScalar::array(value_type)
    } else {
        NTScalar::new(value_type)
    };
    nt.display = true;
    nt.control = true;
    nt.value_alarm = true;
    nt.form = true;
    nt
}

fn snapshot_to_pv_field(snap: &Snapshot) -> PvField {
    // pvxs single-record QSRV builds an NTEnum prototype whenever the
    // backing DBR type is DBR_ENUM (`ioc/singlesource.cpp:200-201`), and
    // `NTEnum::build()` defines `value` as an `enum_t { index, choices }`
    // (`src/nt.cpp:121-131`). A Rust `EpicsValue::Enum` *is* that DBR_ENUM
    // scalar, so it must surface as `epics:nt/NTEnum:1.0` carrying the
    // discoverable `value.choices`, not as a bare numeric NTScalar.
    if let EpicsValue::Enum(v) = &snap.value {
        return build_nt_enum(i32::from(*v), snap);
    }
    fill_nt_scalar(&nt_scalar_for(snap).build(), snap)
}

fn snapshot_to_field_desc(snap: &Snapshot) -> FieldDesc {
    // Mirror `snapshot_to_pv_field`: a DBR_ENUM scalar advertises the
    // NTEnum descriptor (`epics:nt/NTEnum:1.0`) so value and introspection
    // stay in lockstep (pvxs `ioc/singlesource.cpp:200-201`, `src/nt.cpp:121-131`).
    if matches!(&snap.value, EpicsValue::Enum(_)) {
        return nt_enum_desc();
    }
    nt_scalar_for(snap).build()
}

/// Populate the NTScalar shape `desc` declares from `snap`.
///
/// The DESCRIPTOR is the shape's only owner: this starts from
/// [`default_value_for`](crate::pvdata::encode::default_value_for) — which
/// already produces a value matching `desc` leaf for leaf — and overwrites
/// only the leaves the snapshot has data for, coercing each to the type the
/// descriptor declared. A leaf the shape does not carry (`display.limitLow`
/// on a string NT) is simply not found, which is exactly pvxs's own
/// `if(auto x = node["…"])` no-op (`iocsource.cpp:275-310`).
///
/// The leaves left at their descriptor default are the ones pvxs also never
/// fills on this path — `control.minStep`, `valueAlarm.active`, the four
/// `*Severity`, `hysteresis`. They are declared, and zero, and the GET reply
/// leaves them unmarked (`read_checked`), so no client reads a zero as
/// authoritative.
fn fill_nt_scalar(desc: &FieldDesc, snap: &Snapshot) -> PvField {
    let mut v = crate::pvdata::encode::default_value_for(desc);

    set_leaf(
        &mut v,
        "value",
        crate::leaf_convert::epics_value_to_pv_leaf(&snap.value),
    );
    set_leaf(&mut v, "alarm", build_alarm(snap));
    set_leaf(&mut v, "timeStamp", build_timestamp(snap));

    // pvxs `iocsource.cpp:306-308` sets `display.description` from the
    // record's DESC field; the limits/units/precision are the DBR_GR_DOUBLE /
    // DBR_UNITS / DBR_PRECISION metadata one field over.
    if let Some(d) = &snap.display {
        set_numeric(&mut v, "display.limitLow", d.lower_disp_limit);
        set_numeric(&mut v, "display.limitHigh", d.upper_disp_limit);
        set_leaf(
            &mut v,
            "display.description",
            PvField::Scalar(ScalarValue::String(d.description.clone())),
        );
        set_leaf(
            &mut v,
            "display.units",
            PvField::Scalar(ScalarValue::String(d.units.clone())),
        );
        set_leaf(
            &mut v,
            "display.precision",
            PvField::Scalar(ScalarValue::Int(d.precision as i32)),
        );
        // pvxs `iocsource.cpp:300-303` fills the four valueAlarm limits from
        // DBR_AL_DOUBLE; in Rust they live on `DisplayInfo`.
        set_numeric(&mut v, "valueAlarm.lowAlarmLimit", d.lower_alarm_limit);
        set_numeric(&mut v, "valueAlarm.lowWarningLimit", d.lower_warning_limit);
        set_numeric(&mut v, "valueAlarm.highWarningLimit", d.upper_warning_limit);
        set_numeric(&mut v, "valueAlarm.highAlarmLimit", d.upper_alarm_limit);
    }
    if let Some(c) = &snap.control {
        set_numeric(&mut v, "control.limitLow", c.lower_ctrl_limit);
        set_numeric(&mut v, "control.limitHigh", c.upper_ctrl_limit);
    }

    // `IOCSource::initialize` (`iocsource.cpp:39-65`) fills the form menu for
    // every Scalar mapping — the one leaf a read assigns that no DBE class
    // ever posts. `form.index` stays 0 ("Default"): selecting another entry
    // needs the channel's `Q:form` info tag, which this source does not model.
    set_leaf(
        &mut v,
        "display.form.choices",
        PvField::ScalarArray(
            crate::nt::FORM_CHOICES
                .iter()
                .map(|c| ScalarValue::String((*c).into()))
                .collect(),
        ),
    );
    v
}

/// Resolve a dotted field path to its leaf, or `None` when the shape does not
/// carry it — pvxs's `if(auto x = node["…"])` guard.
fn leaf_mut<'a>(v: &'a mut PvField, path: &str) -> Option<&'a mut PvField> {
    let mut cur = v;
    for name in path.split('.') {
        let PvField::Structure(s) = cur else {
            return None;
        };
        cur = s.get_field_mut(name)?;
    }
    Some(cur)
}

/// Assign `val` at `path`. A path the shape does not carry is a no-op.
fn set_leaf(v: &mut PvField, path: &str, val: PvField) {
    if let Some(slot) = leaf_mut(v, path) {
        *slot = val;
    }
}

/// Assign a stored `f64` metadata limit at `path`, coerced to the type the
/// DESCRIPTOR declared for that leaf.
///
/// pvxs types `display`/`control`/`valueAlarm` limits with `value.scalarOf()`
/// (`src/nt.cpp:61-104`), so a `longin` gets `int32_t` limits and an `ai` gets
/// `double` ones. Coercing to the slot's existing variant — which
/// `default_value_for` cut from the descriptor — keeps that typing decision
/// where it belongs, in [`crate::nt::NTScalar`], and out of this function.
fn set_numeric(v: &mut PvField, path: &str, val: f64) {
    let Some(PvField::Scalar(slot)) = leaf_mut(v, path) else {
        return;
    };
    // pvxs `Value::copyIn` (`data.cpp:551-578`) casts the double to the
    // store's 64-bit integer FIRST, then truncates to the leaf's declared
    // width. Rust's `as` saturates where the C++ cast overflows, so a limit
    // that does not fit — `recGblGetAlarmDouble`'s NaN, or
    // `getMaxRangeValues`' 2^64 for a DBF_UINT64 field — landed on a
    // different number than the one QSRV2 serves.
    use crate::pvdata::cpp_cast::{double_to_i64, double_to_u64};
    *slot = match slot {
        ScalarValue::Boolean(_) => ScalarValue::Boolean(val != 0.0),
        ScalarValue::Byte(_) => ScalarValue::Byte(double_to_i64(val) as i8),
        ScalarValue::Short(_) => ScalarValue::Short(double_to_i64(val) as i16),
        ScalarValue::Int(_) => ScalarValue::Int(double_to_i64(val) as i32),
        ScalarValue::Long(_) => ScalarValue::Long(double_to_i64(val)),
        ScalarValue::UByte(_) => ScalarValue::UByte(double_to_u64(val) as u8),
        ScalarValue::UShort(_) => ScalarValue::UShort(double_to_u64(val) as u16),
        ScalarValue::UInt(_) => ScalarValue::UInt(double_to_u64(val) as u32),
        ScalarValue::ULong(_) => ScalarValue::ULong(double_to_u64(val)),
        ScalarValue::Float(_) => ScalarValue::Float(val as f32),
        ScalarValue::Double(_) => ScalarValue::Double(val),
        // A string leaf is never a numeric limit: pvxs's `isnumeric` gate
        // means a string NT declares no limit fields at all.
        ScalarValue::String(_) => return,
    };
}

/// pvxs `NTEnum::build()` (`src/nt.cpp:117-134`): `epics:nt/NTEnum:1.0`
/// with `value` an `enum_t { index: Int32, choices: StringA }`, plus
/// `alarm`, `timeStamp`, and a description-only `display`. Distinct from
/// the NTScalar shape: no `control`/`valueAlarm`, and `display` carries
/// only `description`.
fn nt_enum_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTEnum:1.0".into(),
        fields: vec![
            (
                "value".into(),
                FieldDesc::Structure {
                    struct_id: "enum_t".into(),
                    fields: vec![
                        ("index".into(), FieldDesc::Scalar(ScalarType::Int)),
                        ("choices".into(), FieldDesc::ScalarArray(ScalarType::String)),
                    ],
                },
            ),
            ("alarm".into(), alarm_desc()),
            ("timeStamp".into(), timestamp_desc()),
            (
                "display".into(),
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![("description".into(), FieldDesc::Scalar(ScalarType::String))],
                },
            ),
        ],
    }
}

/// Build an NTEnum value from a DBR_ENUM snapshot. `value.choices` comes
/// from `Snapshot.enums.strings` (pvxs fills it from the DBR enum metadata,
/// `ioc/iocsource.cpp:274-285`); an enum snapshot with no choice metadata
/// yields an empty `choices` array, matching pvxs when the menu is empty.
fn build_nt_enum(index: i32, snap: &Snapshot) -> PvField {
    let choices: Vec<ScalarValue> = snap
        .enums
        .as_ref()
        .map(|e| {
            e.strings
                .iter()
                .map(|s| ScalarValue::String(s.clone()))
                .collect()
        })
        .unwrap_or_default();

    let mut value = PvStructure::new("enum_t");
    value
        .fields
        .push(("index".into(), PvField::Scalar(ScalarValue::Int(index))));
    value
        .fields
        .push(("choices".into(), PvField::ScalarArray(choices)));

    // pvxs NTEnum `display` is the anonymous `Struct("display", {description})`
    // (description only, no limits/units/precision — narrower than the NTScalar display).
    let mut display = PvStructure::new("");
    let description = snap
        .display
        .as_ref()
        .map(|d| d.description.clone())
        .unwrap_or_default();
    display.fields.push((
        "description".into(),
        PvField::Scalar(ScalarValue::String(description)),
    ));

    let mut s = PvStructure::new("epics:nt/NTEnum:1.0");
    s.fields.push(("value".into(), PvField::Structure(value)));
    s.fields.push(("alarm".into(), build_alarm(snap)));
    s.fields.push(("timeStamp".into(), build_timestamp(snap)));
    s.fields
        .push(("display".into(), PvField::Structure(display)));
    PvField::Structure(s)
}

fn alarm_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "alarm_t".into(),
        fields: vec![
            ("severity".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("status".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("message".into(), FieldDesc::Scalar(ScalarType::String)),
        ],
    }
}

fn timestamp_desc() -> FieldDesc {
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
    }
}

fn build_alarm(snap: &Snapshot) -> PvField {
    let mut a = PvStructure::new("alarm_t");
    a.fields.push((
        "severity".into(),
        PvField::Scalar(ScalarValue::Int(snap.alarm.severity as i32)),
    ));
    // pvxs `iocsource.cpp:187-223`: PVA `alarm.status` carries the alarm
    // *class* (NONE/DEVICE/DRIVER/RECORD/DB/UNDEFINED, 0–6), not the raw
    // EPICS condition code. Map before emitting so native clients never
    // see e.g. `LINK_ALARM = 14` in a field whose NT contract is 0–6.
    a.fields.push((
        "status".into(),
        PvField::Scalar(ScalarValue::Int(alarm_status_class(snap.alarm.status))),
    ));
    // pvxs `iocsource.cpp:230-236`, exactly:
    //     if((options & DBR_AMSG) && meta.amsg[0])
    //         node["alarm.message"] = meta.amsg;
    //     else
    //         node["alarm.message"] = meta.status && stsmsg ? stsmsg : "";
    // A non-empty carried amsg (the record's `common.amsg`) wins; else the
    // alarm condition string for a non-zero status; else "". Only
    // mbboDirect sets a UDF amsg ("UDFS", `mbboDirectRecord.c:191`); every
    // other record raises UDF via plain `recGblSetSevr` (empty namsg), so
    // its empty amsg falls through here to the "UDF" condition string —
    // which is what pvxs serves for those records too.
    let message = if !snap.alarm.amsg.is_empty() {
        snap.alarm.amsg.clone()
    } else if snap.alarm.status == alarm_status::NO_ALARM {
        String::new()
    } else {
        alarm_condition_string(snap.alarm.status).to_string()
    };
    a.fields.push((
        "message".into(),
        PvField::Scalar(ScalarValue::String(message.into())),
    ));
    PvField::Structure(a)
}

/// Map a raw EPICS `epicsAlarmCondition` (0–21, `alarm.h`) to the PVA
/// `alarm_t.status` **status class** (NONE/DEVICE/DRIVER/RECORD/DB,
/// otherwise UNDEFINED). PVA NT alarm status is a class field in the
/// 0–6 range, not the raw DB condition code — mirrors pvxs
/// `ioc/iocsource.cpp:187-223`. The same mapping exists in the QSRV
/// bridge path (`epics_bridge_rs::qsrv::pvif::alarm_status_class`); the
/// two are kept in lock-step until a shared home for the condition→class
/// table lands in `epics_base_rs::server::recgbl` alongside
/// [`alarm_condition_string`].
fn alarm_status_class(condition: u16) -> i32 {
    use alarm_status as a;
    match condition {
        a::NO_ALARM => 0, // NONE
        a::READ_ALARM
        | a::WRITE_ALARM
        | a::HIHI_ALARM
        | a::HIGH_ALARM
        | a::LOLO_ALARM
        | a::LOW_ALARM
        | a::STATE_ALARM
        | a::COS_ALARM
        | a::HW_LIMIT_ALARM => 1, // DEVICE
        a::COMM_ALARM | a::TIMEOUT_ALARM | a::UDF_ALARM => 2, // DRIVER
        a::CALC_ALARM | a::SCAN_ALARM | a::LINK_ALARM | a::SOFT_ALARM | a::BAD_SUB_ALARM => 3, // RECORD
        a::DISABLE_ALARM | a::SIMM_ALARM | a::READ_ACCESS_ALARM | a::WRITE_ACCESS_ALARM => 4,  // DB
        _ => 6, // UNDEFINED
    }
}

fn build_timestamp(snap: &Snapshot) -> PvField {
    // pvxs `iocsource.cpp:240-248`: `timeStamp` carries the record's
    // acquisition time and userTag, NOT the serialization wall-clock.
    // `Snapshot.timestamp` is the acquisition `WallTime` (POSIX epoch;
    // the codec already added POSIX_TIME_AT_EPICS_EPOCH on decode) and
    // `Snapshot.user_tag` is the nsec-LSB / pulse-id tag that
    // `apply_nsec_mask` strips out of `nanoseconds` (mirroring
    // pvxs `meta.time.nsec & ~info.nsecMask` for the wire nanoseconds and
    // `meta.time.nsec & info.nsecMask` for userTag). Using `now()` here
    // overwrote the acquisition time with serialization time and zeroed
    // the userTag on every record-backed GET/MONITOR.
    let dur = snap.timestamp.since_unix_epoch();
    let mut t = PvStructure::new("time_t");
    t.fields.push((
        "secondsPastEpoch".into(),
        PvField::Scalar(ScalarValue::Long(dur.as_secs() as i64)),
    ));
    t.fields.push((
        "nanoseconds".into(),
        PvField::Scalar(ScalarValue::Int(dur.subsec_nanos() as i32)),
    ));
    t.fields.push((
        "userTag".into(),
        PvField::Scalar(ScalarValue::Int(snap.user_tag)),
    ));
    PvField::Structure(t)
}

/// The leaves one QSRV read assigns into its `cloneEmpty()` — the exact set
/// a GET reply / PUT_GET readback / monitor seed frames.
///
/// pvxs runs `IOCSource::initialize` then `IOCSource::get(…, Everything, …)`
/// for every single-record read (`singlesource.cpp:283`), which is:
///
/// * [`property_leaves`](crate::nt::property_leaves) — `getProperties`
///   (`iocsource.cpp:252-310`), gated by what the record type supplies;
/// * `timeStamp` + `alarm` + `value` — `getTimeAlarm` / `getScalarValue`
///   (`iocsource.cpp:331-352`);
/// * `display.form.choices`, and `display.form.index` only for a channel on
///   the record's VAL field (`if(dbIsValueField(…))`, `iocsource.cpp:53`) —
///   `initialize`, the one leaf pair a read assigns that no DB event posts.
///
/// A returned path the NT does not carry matches nothing downstream, which is
/// pvxs's `if(auto x = node["…"])` no-op: `value.choices` on a plain scalar,
/// `display.form.*` on a string or NTEnum.
fn read_leaves(snap: &Snapshot, is_value_field: bool) -> Vec<String> {
    // A read IS `IOCSource::get(…, UpdateType::Everything, …)` — the same
    // event-class rule with every class set — so it composes from the one
    // owner rather than listing the leaves a second time. `Everything` is
    // pvxs's own name for `DBE_VALUE|DBE_ALARM|DBE_PROPERTY` (`iocsource.h:40`).
    //
    // An NTEnum's value is a STRUCT, so marking bare `value` would mark both
    // its children — `index` AND `choices` — making the choice list ride the
    // value's own mark and bypass the option bit that owns it. pvxs assigns
    // the enum value through `value.index` alone (`iocsource.cpp:589-593`);
    // `value.choices` is `getProperties`' leaf, gated on `DBR_ENUM_STRS`
    // (already in `property_leaves`). A DTYP whose record type declares no
    // `device()` takes C's `goto nostrs` (`dbAccess.c:176-179`), which clears
    // that bit — so the leaf must be OMITTED, not sent as `{0}[]`. That is the
    // distinction `dbAccess.c:205` draws: *"option data not available.
    // distinct from no_str==0"*.
    let mut leaves = crate::nt::event_leaves(
        epics_base_rs::server::recgbl::EventMask::VALUE
            | epics_base_rs::server::recgbl::EventMask::ALARM
            | epics_base_rs::server::recgbl::EventMask::PROPERTY,
        snap.properties,
        matches!(&snap.value, EpicsValue::Enum(_)),
    );
    leaves.push("display.form.choices".into());
    if is_value_field {
        leaves.push("display.form.index".into());
    }
    leaves
}

// ── ChannelSource impl ────────────────────────────────────────────────────

async fn snapshot_for(db: &PvDatabase, name: &str) -> Option<Snapshot> {
    let (_base, field) = parse_pv_name(name);
    match db.find_entry(name).await? {
        PvEntry::Simple(pv) => Some(pv.snapshot()),
        PvEntry::Record(rec) => {
            let inst = rec.read();
            inst.snapshot_for_field(field)
        }
    }
}

impl ChannelSource for PvDatabaseSource {
    fn access(&self) -> &epics_base_rs::server::access_security::AccessGate {
        &self.gate
    }

    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
        let db = self.db.clone();
        async move {
            let mut names = db.all_record_names().await;
            names.extend(db.all_simple_pv_names().await);
            // Aliases are independently addressable channel names —
            // a PVA client doing channelList must see them so it can
            // connect by alias. has_name and find_entry already
            // resolve aliases on the server side.
            names.extend(db.all_alias_names());
            names
        }
    }

    /// Whether this source will SERVE a channel for `name` — the CREATE gate.
    ///
    /// Stricter than [`Self::searchable`] on purpose, and the asymmetry is
    /// pvxs's. `SingleSource::onSearch` claims a name whenever `dbChannelTest`
    /// resolves it (`ioc/singlesource.cpp:467-472`) — a pure dbd name lookup
    /// (`dbFindRecordPart` + `dbFindFieldPart`, `dbChannel.c:311-343`) with no
    /// type check at all. `onCreate` then builds the value prototype OUTSIDE
    /// its `try`/`catch` (`singlesource.cpp:427-459`), so a field whose DBR
    /// type has no NT — `DBF_NOACCESS` maps to `TypeCode::Null`, which
    /// `NTScalar::build()` refuses — throws past `onCreate`, the source never
    /// claims the channel, and the server answers `Refused to create Channel`
    /// (`src/serverchan.cpp:328-351`). Measured against `softIocPVX`:
    /// `ORACLE:AI.MLOK` is searched successfully and then refused.
    ///
    /// So existence is answered per-FIELD here, not per-record: `has_name` is
    /// the search-side lookup and stops at the record (`database/mod.rs`, "for
    /// UDP search"). Resolving the snapshot is the same gate the CA server
    /// already applies at its own CREATE_CHANNEL — `client_field_value` is
    /// `None` for an unmodeled field, which answers `CREATE_CH_FAIL`
    /// (`epics-ca-rs/src/server/tcp.rs:2630-2642`) — so both protocols now
    /// refuse an unservable field at create rather than claiming it and
    /// failing later with "field introspection unavailable".
    ///
    /// A simple PV has no field to narrow, so it resolves exactly as before.
    fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        let db = self.db.clone();
        let name = name.to_string();
        async move { snapshot_for(&db, &name).await.is_some() }
    }

    /// Whether `name` is SEARCH-advertised — deliberately the looser,
    /// record-level [`PvDatabase::has_name`] rather than [`Self::has_pv`].
    ///
    /// pvxs claims at search everything `dbChannelTest` resolves and only
    /// refuses at create (see [`Self::has_pv`]), so a channel that cannot be
    /// served must still be ANSWERED — otherwise the client never sends
    /// CREATE_CHANNEL and a prompt "Refused to create Channel" degrades into a
    /// silent search timeout, which is a different divergence, not a fix.
    /// Keeping `has_name` here also keeps the search reply cheap: no snapshot
    /// is built for a broadcast that may match nothing.
    fn searchable(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        let db = self.db.clone();
        let name = name.to_string();
        async move { db.has_name(&name).await }
    }

    fn get_introspection(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
        let db = self.db.clone();
        let name = name.to_string();
        async move {
            let snap = snapshot_for(&db, &name).await?;
            Some(snapshot_to_field_desc(&snap))
        }
    }

    fn get_value(&self, name: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let db = self.db.clone();
        let name = name.to_string();
        async move {
            let snap = snapshot_for(&db, &name).await?;
            Some(snapshot_to_pv_field(&snap))
        }
    }

    /// Declare the leaves a QSRV read ASSIGNS, so the GET reply frames only
    /// those.
    ///
    /// pvxs reads into a `cloneEmpty()` and frames it with
    /// `to_wire_valid` (`serverget.cpp:104`), so a leaf nobody assigned never
    /// reaches the wire even though introspection declares it. The port's
    /// `PvField` has no unassigned state, so the subset has to be stated —
    /// and without this override the default said "all of them", which put
    /// `control.minStep`, `valueAlarm.active`, the four `valueAlarm.*Severity`
    /// and `valueAlarm.hysteresis` in front of every Delta client, none of
    /// which pvxs ever assigns.
    ///
    /// The leaf set is the same one the QSRV bridge frames, from the same
    /// owner ([`crate::nt::property_leaves`]) and the same input: the
    /// [`PropertySupport`](epics_base_rs::server::snapshot::PropertySupport)
    /// the DB layer already narrowed to the addressed field. Whether a record
    /// supplies display limits is the record type's answer, not this
    /// source's.
    fn read_checked(
        &self,
        checked: epics_base_rs::server::access_security::AccessChecked,
        _ctx: crate::server_native::ChannelContext,
    ) -> impl std::future::Future<Output = Option<SourceRead>> + Send {
        let db = self.db.clone();
        async move {
            let name = checked.pv_name().to_string();
            let snap = snapshot_for(&db, &name).await?;
            let value = snapshot_to_pv_field(&snap);
            let (_base, field) = parse_pv_name(&name);
            Some(SourceRead::marked(
                value,
                read_leaves(
                    &snap,
                    epics_base_rs::server::database::is_value_field(field),
                ),
            ))
        }
    }

    fn put_value(
        &self,
        name: &str,
        value: PvField,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let db = self.db.clone();
        let name = name.to_string();
        async move {
            // The WRITE gate ran in `put_value_checked` before reaching
            // here, so every failure below is operational (Failed), not a
            // denial.
            //
            // A simple (non-record) PV is a pvxs-style mailbox SharedPV: a
            // full-structure PUT updates the *whole* current value — value,
            // alarm and timeStamp together — not just the `value` leaf
            // (pvxs `serverget.cpp:488-490` hands `onPut` the full decoded
            // Value; `SharedPV::post()` assigns the entire posted value,
            // `sharedpv.cpp:417-432`). Persisting only the value dropped any
            // client-supplied alarm/timeStamp, so a later GET reconstructed
            // them from local defaults. A record-backed channel keeps the
            // field-write path: the record owns its alarm/time through
            // processing, and the client cannot stamp them.
            match db.find_entry(&name).await {
                Some(PvEntry::Simple(pv)) => {
                    let prior = pv.snapshot();
                    let snap = pv_field_to_snapshot(&value, &prior).ok_or_else(|| {
                        OpError::failed("PUT value not representable as EpicsValue")
                    })?;
                    pv.set_snapshot(snap);
                    Ok(())
                }
                // A record-backed channel is an EXTERNAL client put, so it
                // owes C `dbPutField` (`dbAccess.c:1252-1332`), not `dbPut`:
                // the DISP/no-mod gates, the field write, the device write,
                // the Passive process and the monitor post. `put_pv` is the
                // `dbPut` analogue — it writes the field and stops, so a
                // passive record took the value but never processed: UDF
                // stayed set, the timestamp stayed at the EPICS epoch, and
                // no monitor event was ever posted. The CA server already
                // routes its external puts here (`epics-ca-rs`
                // `tcp.rs:3866`); PVA must too.
                //
                // The `_no_notify` entry is the one that means `dbPutField`:
                // C builds a `putNotify` only in `dbPutNotify` (WRITE_NOTIFY).
                // This PUT is non-blocking and has no receiver to await, and
                // parking a wait-set whose receiver is dropped would occupy
                // `RecordInstance::notify` until the record's async work ends.
                Some(PvEntry::Record(_)) => {
                    let scalar = extract_put_value_leaf(&value)
                        .ok_or_else(|| OpError::failed("PUT missing 'value' field"))?;
                    let epics = pv_field_to_epics(&scalar).ok_or_else(|| {
                        OpError::failed("PUT value not representable as EpicsValue")
                    })?;
                    // PUT targets a field; split the name the way `process`
                    // below does so an alias resolves in the database.
                    let (base, field) = parse_pv_name(&name);
                    db.put_record_field_from_ca_no_notify(base, field, epics)
                        .await
                        .map_err(|e| OpError::failed(e.to_string()))
                }
                // Name not in the database: report it rather than falling
                // through to a write that would silently find nothing.
                None => Err(OpError::failed(format!("PUT: no such PV '{name}'"))),
            }
        }
    }

    fn process(&self, name: &str) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let db = self.db.clone();
        let name = name.to_string();
        async move {
            // The WRITE-class gate ran in `process_checked` (trait
            // default) before reaching here, so a failure below is
            // operational, not a denial. pvxs serves PVA PROCESS by
            // running the IOC processing chain (`iocsource.cpp:397-417`
            // dbProcess / `singlesource.cpp:346-382`); the backing
            // database already exposes that through
            // `process_record_with_links` — the foreign-caller
            // full-processing entry that is alias-aware, takes the
            // record's advisory write gate, and runs
            // INP -> process -> alarms -> OUT -> FLNK with cycle/depth
            // guards. The previous no-op default returned success while
            // the record never processed (alarms, monitors, OUT links and
            // the FLNK chain all skipped).
            match db.find_entry(&name).await {
                Some(PvEntry::Record(_)) => {
                    // PROCESS targets the whole record, not a field —
                    // drop any `.FIELD` suffix so the record/alias name
                    // resolves in `process_record_with_links`.
                    let (base, _field) = parse_pv_name(&name);
                    let base = base.to_string();
                    let mut visited = std::collections::HashSet::new();
                    db.process_record_with_links(&base, &mut visited, 0)
                        .await
                        .map_err(|e| OpError::failed(e.to_string()))
                }
                // A simple/mailbox SharedPV has no record body to run;
                // report PROCESS as unsupported rather than silently
                // succeeding (no process hook exists for these).
                Some(PvEntry::Simple(_)) => Err(OpError::failed(format!(
                    "PROCESS not supported for simple PV '{name}'"
                ))),
                None => Err(OpError::failed(format!("no record serves '{name}'"))),
            }
        }
    }

    // the legacy `get_value_ctx` / `subscribe_ctx` /
    // `put_value_ctx` overrides were deleted. The trait's
    // `*_checked` defaults already enforce the AccessChecked level
    // before delegating to the ctx-less variants here, and the
    // gate (built by `Self::build_gate`) reads the ASG/ASL from
    // the record itself — so this source no longer carries any
    // duplicate ACF logic.

    fn is_writable(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        let db = self.db.clone();
        let name = name.to_string();
        async move { db.has_name(&name).await }
    }

    /// A record-backed monitor UPDATE marks only the leaves its own `DBE_*`
    /// class covers, which is what `MonitorUpdate::marked` exists to say.
    ///
    /// pvxs reads each event's real mask from `pDbFieldLog->mask`
    /// (`singlesource.cpp:80-84`) and hands it to `IOCSource::get` as the
    /// `UpdateType`, so a value post assigns value+timeStamp while the
    /// metadata leaves are assigned only by the separate `DBE_PROPERTY`
    /// subscription (`:162-166`). The default `marked: None` framed the full
    /// mask on every update, so the port put display/control/valueAlarm on
    /// the wire for every value change — a strict superset of C.
    ///
    /// [`crate::nt::event_leaves`] is the single owner of that rule; this
    /// method only supplies the per-event mask it keys on.
    fn subscribe_checked_opts_marked(
        &self,
        checked: epics_base_rs::server::access_security::AccessChecked,
        ctx: crate::server_native::ChannelContext,
        opts: crate::server_native::source::MonitorOptions,
    ) -> impl std::future::Future<
        Output = Option<MonitorStream<crate::server_native::source::MonitorUpdate>>,
    > + Send {
        let db = self.db.clone();
        async move {
            if !checked.allows_read() {
                return None;
            }
            let name = checked.pv_name().to_string();
            // A mailbox SharedPV posts a wholly-assigned value and has no
            // record events to classify — keep the unmarked default.
            if !matches!(db.find_entry(&name).await?, PvEntry::Record(_)) {
                return self
                    .subscribe_checked_opts(checked, ctx, opts)
                    .await
                    .map(crate::server_native::source::plain_monitor_updates);
            }

            use epics_base_rs::server::database::db_access::DbSubscription;
            // The union of pvxs's two subscriptions (value + property), so the
            // same events arrive; each is narrowed by its own posted mask
            // below. The prior VALUE|LOG subscription could not deliver a
            // DBE_PROPERTY event at all.
            let sub = DbSubscription::subscribe_with_mask(
                &db,
                &name,
                0,
                crate::nt::monitor_mask().bits(),
            )
            .await?;
            // The subscription IS the stream: `marked_update` runs as the
            // server pulls, so no task stands between the two.
            Some(MonitorStream::Upstream(UpstreamMonitor::from_db(
                sub,
                marked_update,
            )))
        }
    }

    fn subscribe(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
        let db = self.db.clone();
        let name = name.to_string();
        async move {
            let entry = db.find_entry(&name).await?;
            match entry {
                PvEntry::Simple(pv) => {
                    // Register the live subscriber BEFORE reading the
                    // initial snapshot so a PUT racing between the two is
                    // delivered through the stream, not missed. pvxs
                    // `SharedPV::post()` fans every later value out to its
                    // stored subscribers (`sharedpv.cpp:417-440`); the
                    // simple-PV `PvSubscription` is the same mechanism —
                    // `ProcessVariable::set` -> `notify_subscribers`.
                    use epics_base_rs::server::pv::PvSubscription;
                    match PvSubscription::subscribe(pv.clone()).await {
                        Some(sub) => {
                            let initial = snapshot_to_pv_field(&pv.snapshot());
                            // The seed rides on the stream and is handed out
                            // ahead of the subscription's own events, exactly
                            // as the bridge's `tx.send(initial)` did before it
                            // entered the loop. Dropping the stream drops
                            // `sub`, whose `Drop` removes the subscriber slot
                            // from the ProcessVariable.
                            Some(MonitorStream::Upstream(
                                UpstreamMonitor::from_pv(sub, event_field).with_seed(initial),
                            ))
                        }
                        None => {
                            // Per-PV subscriber cap reached: still honour the
                            // connect-time read so the client at least sees
                            // the current value, then end the stream.
                            let (tx, rx) = mpsc::channel::<PvField>(1);
                            let _ = tx.send(snapshot_to_pv_field(&pv.snapshot())).await;
                            Some(MonitorStream::Channel(rx))
                        }
                    }
                }
                PvEntry::Record(_rec) => {
                    // Subscribe via the public DbSubscription API.
                    use epics_base_rs::server::database::db_access::DbSubscription;
                    let sub = DbSubscription::subscribe(&db, &name).await?;
                    Some(MonitorStream::Upstream(UpstreamMonitor::from_db(
                        sub,
                        event_field,
                    )))
                }
            }
        }
    }
}

/// The per-event transform the record-backed marked-monitor bridge task used
/// to apply. A free `fn` so it can be a [`UpstreamMonitor`] map pointer.
///
/// `None` (the bridge's `continue`) is an event whose `DBE_*` class marks no
/// leaf — no wire meaning, since C would have assigned none either.
fn marked_update(
    ev: epics_base_rs::server::pv::MonitorEvent,
) -> Option<crate::server_native::source::MonitorUpdate> {
    let marked = crate::nt::event_leaves(
        ev.mask,
        ev.snapshot.properties,
        matches!(&ev.snapshot.value, EpicsValue::Enum(_)),
    );
    if marked.is_empty() {
        return None;
    }
    Some(crate::server_native::source::MonitorUpdate {
        marked: Some(marked),
        ..crate::server_native::source::MonitorUpdate::from(snapshot_to_pv_field(&ev.snapshot))
    })
}

/// The unmarked transform of the two plain `subscribe` bridge tasks: every
/// event yields its snapshot as a `PvField`, none are filtered.
fn event_field(ev: epics_base_rs::server::pv::MonitorEvent) -> Option<PvField> {
    Some(snapshot_to_pv_field(&ev.snapshot))
}

// ── PvField → EpicsValue (PUT path) ────────────────────────────────────

/// Extract the value leaf of a PUT `PvField`: the `value` member of an
/// NT structure, or the field itself for a bare scalar/array. An NTEnum
/// PUT carries `value` as an `enum_t { index, choices }`; pvxs writes
/// through `value.index` (`ioc/iocsource.cpp:589-593`).
fn extract_put_value_leaf(value: &PvField) -> Option<PvField> {
    match value {
        PvField::Structure(s) => match s.get_field("value") {
            Some(PvField::Structure(inner)) => inner.get_field("index").cloned(),
            other => other.cloned(),
        },
        other => Some(other.clone()),
    }
}

/// Read an integer-typed scalar field of a structure as `i64`, tolerating
/// whatever integer width the client encoded (NTScalar spec uses `int`
/// for alarm severity/status/userTag and `long` for secondsPastEpoch).
fn scalar_field_i64(s: &PvStructure, field: &str) -> Option<i64> {
    match s.get_field(field)? {
        PvField::Scalar(sv) => match sv {
            ScalarValue::Byte(x) => Some(i64::from(*x)),
            ScalarValue::Short(x) => Some(i64::from(*x)),
            ScalarValue::Int(x) => Some(i64::from(*x)),
            ScalarValue::Long(x) => Some(*x),
            ScalarValue::UByte(x) => Some(i64::from(*x)),
            ScalarValue::UShort(x) => Some(i64::from(*x)),
            ScalarValue::UInt(x) => Some(i64::from(*x)),
            ScalarValue::ULong(x) => i64::try_from(*x).ok(),
            _ => None,
        },
        _ => None,
    }
}

/// Build a full `Snapshot` from a mailbox PUT's NT structure: value plus
/// any explicitly-supplied `alarm` (severity/status) and `timeStamp`
/// (secondsPastEpoch/nanoseconds/userTag). Sub-fields the client did not
/// supply carry over from `prior` (which, after the delta merge, already
/// reflects the last posted value for unmarked members).
fn pv_field_to_snapshot(value: &PvField, prior: &Snapshot) -> Option<Snapshot> {
    let leaf = extract_put_value_leaf(value)?;
    let epics = pv_field_to_epics(&leaf)?;
    let mut snap = prior.clone();
    snap.value = epics;
    if let PvField::Structure(s) = value {
        if let Some(PvField::Structure(alarm)) = s.get_field("alarm") {
            if let Some(sev) = scalar_field_i64(alarm, "severity") {
                snap.alarm.severity = sev as u16;
            }
            if let Some(status) = scalar_field_i64(alarm, "status") {
                snap.alarm.status = status as u16;
            }
        }
        if let Some(PvField::Structure(ts)) = s.get_field("timeStamp") {
            if let (Some(secs), nanos) = (
                scalar_field_i64(ts, "secondsPastEpoch"),
                scalar_field_i64(ts, "nanoseconds").unwrap_or(0),
            ) {
                if secs >= 0 {
                    let nanos = nanos.clamp(0, 999_999_999) as u32;
                    // Inject the exact wire integers; a `SystemTime` would
                    // round `nanos` to 100 ns on Windows, dropping the low
                    // nsec a PVA PUT's `timeStamp.nanoseconds` carries.
                    snap.timestamp = WallTime::from_unix(secs as u64, nanos);
                }
            }
            if let Some(tag) = scalar_field_i64(ts, "userTag") {
                snap.user_tag = tag as i32;
            }
        }
    }
    Some(snap)
}

fn pv_field_to_epics(field: &PvField) -> Option<EpicsValue> {
    match field {
        PvField::Scalar(sv) => Some(scalar_to_epics(sv)),
        PvField::ScalarArray(items) => scalar_array_to_epics(items),
        // the PVA wire decoder delivers a decoded scalar array
        // as the refcount-shared `ScalarArrayTyped` form, not the
        // generic `ScalarArray`. Convert it directly from the typed
        // variant — never erase the element tag through
        // `to_scalar_values()` first. The earlier code did exactly
        // that and then recovered the type from the slice's first
        // element, so an empty typed array (`double[] {}`, `string[]
        // {}`) lost its type and was rejected, even though
        // `TypedScalarArray` still carries its `ScalarType` at length
        // zero. A PVA client could therefore not clear a waveform by
        // PUTing an empty typed array. pvxs encodes/decodes scalar
        // arrays by descriptor type, not by inspecting the first
        // element (`dataencode.cpp:315-352`), so a zero-length typed
        // array keeps its element type.
        PvField::ScalarArrayTyped(t) => Some(typed_array_to_epics(t)),
        _ => None,
    }
}

/// Convert a wire-decoded [`TypedScalarArray`] directly to the matching
/// [`EpicsValue`] array, preserving the element type at every length
/// (including zero). The per-variant element mapping mirrors
/// [`scalar_to_epics`] exactly, so a typed array and a scalar of the
/// same PVA type land in the same `EpicsValue` family — there is no
/// length boundary where the rule changes.
fn typed_array_to_epics(t: &TypedScalarArray) -> EpicsValue {
    match t {
        TypedScalarArray::Double(a) => EpicsValue::DoubleArray(a.to_vec()),
        TypedScalarArray::Float(a) => EpicsValue::FloatArray(a.to_vec()),
        TypedScalarArray::Int(a) => EpicsValue::LongArray(a.to_vec()),
        TypedScalarArray::Long(a) => EpicsValue::Int64Array(a.to_vec()),
        // PVA `uint`/`uint[]` carries the full `epicsUInt32` range; the
        // Rust value model has no unsigned-32 scalar, so `Int64Array`
        // is the lossless carrier (same rule as the scalar `UInt` arm).
        TypedScalarArray::UInt(a) => EpicsValue::Int64Array(a.iter().map(|x| *x as i64).collect()),
        // PVA `ulong[]` → `UInt64Array` keeps the full unsigned 64-bit width.
        TypedScalarArray::ULong(a) => EpicsValue::UInt64Array(a.to_vec()),
        TypedScalarArray::Short(a) => EpicsValue::ShortArray(a.to_vec()),
        // Byte/UByte → Char (bit-preserving), UShort/Boolean → Enum —
        // the same element rules `scalar_to_epics` applies to the
        // corresponding scalars.
        TypedScalarArray::Byte(a) => EpicsValue::CharArray(a.iter().map(|x| *x as u8).collect()),
        TypedScalarArray::UByte(a) => EpicsValue::CharArray(a.to_vec()),
        TypedScalarArray::UShort(a) => EpicsValue::EnumArray(a.to_vec()),
        TypedScalarArray::Boolean(a) => {
            EpicsValue::EnumArray(a.iter().map(|b| u16::from(*b)).collect())
        }
        TypedScalarArray::String(a) => EpicsValue::StringArray(a.to_vec()),
    }
}

fn scalar_array_to_epics(items: &[ScalarValue]) -> Option<EpicsValue> {
    if items.is_empty() {
        return None;
    }
    match &items[0] {
        ScalarValue::Double(_) => Some(EpicsValue::DoubleArray(
            items
                .iter()
                .filter_map(|v| match v {
                    ScalarValue::Double(x) => Some(*x),
                    _ => None,
                })
                .collect(),
        )),
        ScalarValue::Int(_) => Some(EpicsValue::LongArray(
            items
                .iter()
                .filter_map(|v| match v {
                    ScalarValue::Int(x) => Some(*x),
                    _ => None,
                })
                .collect(),
        )),
        ScalarValue::Long(_) => Some(EpicsValue::Int64Array(
            items
                .iter()
                .filter_map(|v| match v {
                    ScalarValue::Long(x) => Some(*x),
                    _ => None,
                })
                .collect(),
        )),
        // PVA `uint[]` has no array arm here, so a `DBF_ULONG`
        // waveform PUT fell through to `None` and was rejected as
        // "PUT value not representable as EpicsValue". Mirror the
        // scalar `UInt -> Int64` rule: `Int64Array` carries the full
        // `epicsUInt32` range of every element losslessly.
        ScalarValue::UInt(_) => Some(EpicsValue::Int64Array(
            items
                .iter()
                .filter_map(|v| match v {
                    ScalarValue::UInt(x) => Some(*x as i64),
                    _ => None,
                })
                .collect(),
        )),
        ScalarValue::Float(_) => Some(EpicsValue::FloatArray(
            items
                .iter()
                .filter_map(|v| match v {
                    ScalarValue::Float(x) => Some(*x),
                    _ => None,
                })
                .collect(),
        )),
        // PVA `ulong[]` has no array arm here, so a
        // `DBF_UINT64` waveform PUT fell through to `None` and was
        // rejected as "PUT value not representable as EpicsValue".
        // Preserve the unsigned 64-bit elements as
        // `EpicsValue::UInt64Array`; `convert_to(DBF_UINT64)` is
        // then a no-op and any other array target still coerces.
        ScalarValue::ULong(_) => Some(EpicsValue::UInt64Array(
            items
                .iter()
                .filter_map(|v| match v {
                    ScalarValue::ULong(x) => Some(*x),
                    _ => None,
                })
                .collect(),
        )),
        _ => None,
    }
}

fn scalar_to_epics(v: &ScalarValue) -> EpicsValue {
    match v {
        ScalarValue::Boolean(b) => EpicsValue::Enum(if *b { 1 } else { 0 }),
        ScalarValue::Byte(x) => EpicsValue::Char(*x as u8),
        ScalarValue::Short(x) => EpicsValue::Short(*x),
        ScalarValue::Int(x) => EpicsValue::Long(*x),
        ScalarValue::Long(x) => EpicsValue::Int64(*x),
        ScalarValue::UByte(x) => EpicsValue::Char(*x),
        ScalarValue::UShort(x) => EpicsValue::Enum(*x),
        // PVA `uint` is unsigned 32-bit. Casting it through `i32`
        // sign-flips `0x8000_0000..=0xffff_ffff` to negative before
        // `PvDatabase::put_pv` can coerce to the target field. The
        // Rust value model has no unsigned-32 scalar; `Int64` carries
        // the full `epicsUInt32` range losslessly, matching the
        // `DBF_ULONG` convention used by the `ts` filter
        // (`server/database/filters/ts.rs`) and pvxs's
        // `uint32_t -> uint64_t` storage (`pvxs/src/pvxs/data.h:64-67`).
        ScalarValue::UInt(x) => EpicsValue::Int64(*x as i64),
        // PVA `ulong` is unsigned 64-bit. Narrowing it to
        // `EpicsValue::Long` (i32) here drops the upper 32 bits before
        // `PvDatabase::put_pv` can coerce to the target `DBF_UINT64`
        // field. Preserve the full width as `EpicsValue::UInt64`; the
        // database's `convert_to(DBF_UINT64)` is then a no-op
        // (`db_field_type()` already matches), and any other target
        // field type still coerces faithfully from the unsigned value.
        ScalarValue::ULong(x) => EpicsValue::UInt64(*x),
        ScalarValue::Float(x) => EpicsValue::Float(*x),
        ScalarValue::Double(x) => EpicsValue::Double(*x),
        ScalarValue::String(s) => EpicsValue::String(s.clone()),
    }
}

#[allow(unused_imports)]
use crate::error::PvaError;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_native::ChannelContext;
    use epics_base_rs::server::access_security::parse_acf;
    use epics_base_rs::server::snapshot::PropertySupport;
    use epics_base_rs::types::PvString;

    fn make_ctx(host: &str, account: &str, method: &str) -> ChannelContext {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        ChannelContext {
            peer: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0),
            creds: std::sync::Arc::new(crate::server_native::config::ClientCredentials {
                account: account.to_string(),
                method: method.to_string(),
                host: host.to_string(),
                authority: String::new(),
                roles: Vec::new(),
            }),
            pv_request: None,
            log: Default::default(),
        }
    }

    /// Test-only compat layer that reproduces the
    /// pre-refactor `*_ctx` shape on top of the new gate +
    /// `*_checked` typed API. Production callers in `tcp.rs` go
    /// through the gate directly; only these regression tests
    /// retain the legacy call shape for clarity.
    trait PvaSourceTestExt {
        async fn put_value_ctx(
            &self,
            pv: &str,
            value: PvField,
            ctx: ChannelContext,
        ) -> Result<(), String>;
        async fn get_value_ctx(&self, pv: &str, ctx: ChannelContext) -> Option<PvField>;
        async fn subscribe_ctx(
            &self,
            pv: &str,
            ctx: ChannelContext,
        ) -> Option<MonitorStream<PvField>>;
    }

    impl PvaSourceTestExt for PvDatabaseSource {
        async fn put_value_ctx(
            &self,
            pv: &str,
            value: PvField,
            ctx: ChannelContext,
        ) -> Result<(), String> {
            let checked = self
                .access()
                .check(
                    pv,
                    &ctx.creds.host,
                    &ctx.creds.account,
                    &ctx.creds.method,
                    "",
                )
                .await;
            self.put_value_checked(checked, value, ctx)
                .await
                .map_err(|e| e.message)
        }
        async fn get_value_ctx(&self, pv: &str, ctx: ChannelContext) -> Option<PvField> {
            let checked = self
                .access()
                .check(
                    pv,
                    &ctx.creds.host,
                    &ctx.creds.account,
                    &ctx.creds.method,
                    "",
                )
                .await;
            self.get_value_checked(checked, ctx).await
        }
        async fn subscribe_ctx(
            &self,
            pv: &str,
            ctx: ChannelContext,
        ) -> Option<MonitorStream<PvField>> {
            let checked = self
                .access()
                .check(
                    pv,
                    &ctx.creds.host,
                    &ctx.creds.account,
                    &ctx.creds.method,
                    "",
                )
                .await;
            self.subscribe_checked(checked, ctx).await
        }
    }

    fn pv_double(v: f64) -> PvField {
        // Top-level scalar PvField — put_value / put_value_ctx accept
        // both NTScalar and bare scalar.
        PvField::Scalar(ScalarValue::Double(v))
    }

    /// The seven leaves the NT declares but pvxs never assigns on a read.
    /// Marking any of them tells a client a fabricated zero is authoritative.
    const NEVER_MARKED: [&str; 7] = [
        "control.minStep",
        "valueAlarm.active",
        "valueAlarm.lowAlarmSeverity",
        "valueAlarm.lowWarningSeverity",
        "valueAlarm.highWarningSeverity",
        "valueAlarm.highAlarmSeverity",
        "valueAlarm.hysteresis",
    ];

    fn leaves_for(props: PropertySupport, is_value_field: bool) -> Vec<String> {
        let mut snap = Snapshot::new(EpicsValue::Double(0.0), 0, 0, std::time::UNIX_EPOCH);
        snap.properties = props;
        read_leaves(&snap, is_value_field)
    }

    fn enum_leaves_for(props: PropertySupport) -> Vec<String> {
        let mut snap = Snapshot::new(EpicsValue::Enum(0), 0, 0, std::time::UNIX_EPOCH);
        snap.properties = props;
        read_leaves(&snap, false)
    }

    /// An NTEnum's `value` is a STRUCT. Marking bare `value` would mark both
    /// children, carrying `choices` in on the value's mark and bypassing the
    /// `DBR_ENUM_STRS` bit that owns it. pvxs assigns the enum through
    /// `value.index` (`iocsource.cpp:589-593`), so the mark must be the index
    /// leaf — never the parent.
    #[test]
    fn an_enum_read_marks_the_index_leaf_not_the_value_parent() {
        let leaves = enum_leaves_for(PropertySupport::NONE);
        assert!(
            leaves.contains(&"value.index".to_string()),
            "the enum value must be marked through its index leaf, got {leaves:?}"
        );
        assert!(
            !leaves.contains(&"value".to_string()),
            "marking the `value` parent would drag `choices` in with it: {leaves:?}"
        );
    }

    /// C `dbAccess.c:176-179`: a `DTYP` whose record type declares no
    /// `device()` has `ftPvt == NULL` and takes `goto nostrs`, clearing
    /// `DBR_ENUM_STRS`. QSRV2 then OMITS `value.choices` rather than sending it
    /// empty — `dbAccess.c:205`: *"option data not available. distinct from
    /// no_str==0"*. Measured: `ORACLE:CALC.DTYP` shipped `{0}[]` where C sends
    /// nothing.
    #[test]
    fn an_enum_with_no_enum_strs_omits_choices_entirely() {
        let leaves = enum_leaves_for(PropertySupport::NONE);
        // Both halves are load-bearing: a bare `value` mark COVERS
        // `value.choices` without ever naming it, which is exactly how the
        // measured `{0}[]` shipped. Asserting only the absence of the
        // `value.choices` string passes on the defective code.
        assert!(
            !leaves.contains(&"value.choices".to_string()),
            "no DBR_ENUM_STRS => the choices leaf is not marked: {leaves:?}"
        );
        assert!(
            !leaves.contains(&"value".to_string()),
            "nor may it be covered by a mark on the `value` parent: {leaves:?}"
        );
    }

    /// The other side of the same distinction: `ai` DOES declare device
    /// support, so its `DTYP` supplies the bit and both leaves are marked.
    #[test]
    fn an_enum_that_supplies_enum_strs_marks_index_and_choices() {
        let leaves = enum_leaves_for(PropertySupport {
            enum_strs: true,
            ..PropertySupport::NONE
        });
        assert!(leaves.contains(&"value.index".to_string()));
        assert!(leaves.contains(&"value.choices".to_string()));
    }

    /// A plain scalar's `value` IS the leaf — it must keep its bare mark; the
    /// index split is the enum shape's alone.
    #[test]
    fn a_scalar_read_still_marks_the_bare_value_leaf() {
        let leaves = leaves_for(PropertySupport::NUMERIC, true);
        assert!(leaves.contains(&"value".to_string()));
        assert!(!leaves.contains(&"value.index".to_string()));
    }

    /// The invariant, over every one of the 64 possible rset support masks:
    /// a read never marks a leaf `getProperties` does not assign.
    #[test]
    fn a_read_never_marks_the_seven_unassigned_leaves() {
        for bits in 0u8..64 {
            let props = PropertySupport {
                units: bits & 1 != 0,
                precision: bits & 2 != 0,
                graphic_double: bits & 4 != 0,
                control_double: bits & 8 != 0,
                alarm_double: bits & 16 != 0,
                enum_strs: bits & 32 != 0,
            };
            for vf in [true, false] {
                let leaves = leaves_for(props, vf);
                for n in NEVER_MARKED {
                    assert!(
                        !leaves.contains(&n.to_string()),
                        "mask {bits:06b} (is_value_field={vf}) marked {n}",
                    );
                }
            }
        }
    }

    /// `ai`: every rset slot present, `DBF_DOUBLE` VAL. Pinned against the
    /// `softIocPVX` delta the oracle measured for `record(ai,"…"){}`.
    #[test]
    fn full_numeric_record_marks_the_measured_pvxs_leaf_set() {
        let mut got = leaves_for(PropertySupport::NUMERIC, true);
        got.sort();
        let mut want: Vec<String> = [
            "value",
            "alarm",
            "timeStamp",
            "display.limitLow",
            "display.limitHigh",
            "display.description",
            "display.units",
            "display.precision",
            "display.form.index",
            "display.form.choices",
            "control.limitLow",
            "control.limitHigh",
            "valueAlarm.lowAlarmLimit",
            "valueAlarm.lowWarningLimit",
            "valueAlarm.highWarningLimit",
            "valueAlarm.highAlarmLimit",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        want.sort();
        assert_eq!(got, want);
    }

    /// `longin`: the narrowed mask arrives with `precision: false` because
    /// `dbGet` clears `DBR_PRECISION` for a non-float field, so no
    /// `display.precision` is marked even though the NT declares it.
    /// Measured: pvxs prints no `display.precision` for a `longin`.
    #[test]
    fn integer_record_marks_limits_but_not_precision() {
        let leaves = leaves_for(
            PropertySupport {
                precision: false,
                ..PropertySupport::NUMERIC
            },
            true,
        );
        assert!(leaves.contains(&"display.limitLow".to_string()));
        assert!(!leaves.contains(&"display.precision".to_string()));
    }

    /// `stringin`/`stringout`: no rset property slot at all, so the only
    /// display leaf marked is `description` — NOT `units`, which the string
    /// NT declares. Measured: pvxs prints `display.description` alone.
    #[test]
    fn record_with_no_property_slots_marks_only_description() {
        let leaves = leaves_for(PropertySupport::NONE, true);
        assert!(leaves.contains(&"display.description".to_string()));
        assert!(!leaves.contains(&"display.units".to_string()));
        assert!(!leaves.contains(&"display.limitLow".to_string()));
        assert!(!leaves.contains(&"control.limitLow".to_string()));
    }

    /// `display.form.index` is `initialize`'s VAL-only leaf
    /// (`if(dbIsValueField(…))`, `iocsource.cpp:53`): a channel on `REC.RVAL`
    /// gets the choices but not the index. `choices` is marked either way.
    #[test]
    fn form_index_is_marked_only_for_a_val_channel() {
        let val = leaves_for(PropertySupport::NUMERIC, true);
        assert!(val.contains(&"display.form.index".to_string()));
        assert!(val.contains(&"display.form.choices".to_string()));

        let rval = leaves_for(PropertySupport::NUMERIC, false);
        assert!(!rval.contains(&"display.form.index".to_string()));
        assert!(rval.contains(&"display.form.choices".to_string()));
    }

    /// An enum record (`bi`/`mbbi`) supplies `DBR_ENUM_STRS`, so the read
    /// marks `value.choices` — the leaf the NTEnum shape carries and the
    /// plain-scalar shape does not.
    #[test]
    fn enum_record_marks_value_choices() {
        let leaves = leaves_for(
            PropertySupport {
                enum_strs: true,
                ..PropertySupport::NONE
            },
            true,
        );
        assert!(leaves.contains(&"value.choices".to_string()));
    }

    /// Look up a scalar sub-field of a timeStamp/display/valueAlarm meta
    /// structure by name, for the NT-meta synthesizer regression tests.
    fn scalar(s: &PvStructure, name: &str) -> ScalarValue {
        match &s
            .fields
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("missing field {name}"))
            .1
        {
            PvField::Scalar(v) => v.clone(),
            other => panic!("field {name} is not a scalar: {other:?}"),
        }
    }

    /// Names of `desc`'s immediate children, or panic if it is not a struct.
    fn child_names(desc: &FieldDesc) -> Vec<String> {
        let FieldDesc::Structure { fields, .. } = desc else {
            panic!("expected a structure, got {desc:?}");
        };
        fields.iter().map(|(n, _)| n.clone()).collect()
    }

    fn child<'a>(desc: &'a FieldDesc, name: &str) -> &'a FieldDesc {
        let FieldDesc::Structure { fields, .. } = desc else {
            panic!("expected a structure, got {desc:?}");
        };
        &fields
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("missing member {name}"))
            .1
    }

    fn desc_of(v: EpicsValue) -> FieldDesc {
        snapshot_to_field_desc(&Snapshot::new(v, 0, 0, std::time::UNIX_EPOCH))
    }

    /// `display.form` — pvxs has emitted it for every numeric NTScalar since
    /// 1.2.0, because QSRV passes `form=true` (`ioc/singlesource.cpp:203`,
    /// `src/nt.cpp:67-77`). Measured absent from the port against
    /// `softIocPVX` on `ai`.
    #[test]
    fn numeric_nt_declares_display_form_enum() {
        let desc = desc_of(EpicsValue::Double(0.0));
        let display = child(&desc, "display");
        assert_eq!(
            child_names(display),
            [
                "limitLow",
                "limitHigh",
                "description",
                "units",
                "precision",
                "form"
            ],
        );
        let form = child(display, "form");
        let FieldDesc::Structure { struct_id, .. } = form else {
            panic!("form must be a structure");
        };
        assert_eq!(struct_id, "enum_t");
        assert!(matches!(
            child(form, "index"),
            FieldDesc::Scalar(ScalarType::Int)
        ));
        assert!(matches!(
            child(form, "choices"),
            FieldDesc::ScalarArray(ScalarType::String)
        ));
    }

    /// `valueAlarm.hysteresis` is `Float64` — `src/nt.cpp:109`. The port
    /// declared `uint8_t`, which is a different wire type in the same slot.
    #[test]
    fn value_alarm_hysteresis_is_double() {
        let desc = desc_of(EpicsValue::Double(0.0));
        let va = child(&desc, "valueAlarm");
        assert!(matches!(
            child(va, "hysteresis"),
            FieldDesc::Scalar(ScalarType::Double)
        ));
    }

    /// pvxs cuts every limit from `value.scalarOf()` (`Member(scalar,
    /// "limitLow")`, `src/nt.cpp:61-104`), so a `longin` advertises `int32_t`
    /// limits — not the `double` the port hard-coded. Measured against
    /// `softIocPVX`: `display.limitLow int32_t = 0`.
    #[test]
    fn integer_nt_limits_take_the_value_type() {
        let desc = desc_of(EpicsValue::Long(0));
        for (parent, leaf) in [
            ("display", "limitLow"),
            ("display", "limitHigh"),
            ("control", "limitLow"),
            ("control", "minStep"),
            ("valueAlarm", "lowAlarmLimit"),
            ("valueAlarm", "highAlarmLimit"),
        ] {
            assert!(
                matches!(
                    child(child(&desc, parent), leaf),
                    FieldDesc::Scalar(ScalarType::Int)
                ),
                "{parent}.{leaf} must be int32_t for a DBF_LONG value",
            );
        }
        // ...but hysteresis is Float64 regardless of the value type, and
        // precision stays Int32.
        assert!(matches!(
            child(child(&desc, "valueAlarm"), "hysteresis"),
            FieldDesc::Scalar(ScalarType::Double)
        ));
        assert!(matches!(
            child(child(&desc, "display"), "precision"),
            FieldDesc::Scalar(ScalarType::Int)
        ));
    }

    /// pvxs's `isnumeric` gate (`src/nt.cpp:55`): a string value carries
    /// `display = {description, units}` and NO `control` / `valueAlarm` at
    /// all. The port declared the full numeric shape for `stringin`/
    /// `stringout`, which is what made those two cases diverge.
    #[test]
    fn string_nt_omits_control_value_alarm_and_numeric_display() {
        let desc = desc_of(EpicsValue::String("x".into()));
        let names = child_names(&desc);
        assert_eq!(names, ["value", "alarm", "timeStamp", "display"]);
        assert_eq!(
            child_names(child(&desc, "display")),
            ["description", "units"]
        );
    }

    /// A numeric ARRAY is the same `NTScalar::build()` with an array value
    /// type, so it keeps `control`/`valueAlarm` with ELEMENT-typed limits
    /// (pvxs `src/nt.cpp:44-112`). The port dropped both for every array.
    #[test]
    fn numeric_array_nt_keeps_control_and_value_alarm() {
        let desc = desc_of(EpicsValue::LongArray(vec![1, 2]));
        let FieldDesc::Structure { struct_id, .. } = &desc else {
            panic!("expected a structure");
        };
        assert_eq!(struct_id, "epics:nt/NTScalarArray:1.0");
        assert_eq!(
            child_names(&desc),
            [
                "value",
                "alarm",
                "timeStamp",
                "display",
                "control",
                "valueAlarm"
            ],
        );
        assert!(matches!(
            child(child(&desc, "control"), "limitLow"),
            FieldDesc::Scalar(ScalarType::Int)
        ));
    }

    /// The served value and the advertised type are projections of one
    /// configuration, so every leaf of one must exist, with the same type, in
    /// the other. This is the invariant the two hand-rolled builders could
    /// not hold.
    #[test]
    fn value_and_introspection_agree_for_every_value_kind() {
        for v in [
            EpicsValue::Double(1.0),
            EpicsValue::Long(1),
            EpicsValue::Short(1),
            EpicsValue::Char(1),
            EpicsValue::String("s".into()),
            EpicsValue::DoubleArray(vec![1.0]),
            EpicsValue::StringArray(vec!["s".into()]),
        ] {
            let snap = Snapshot::new(v.clone(), 0, 0, std::time::UNIX_EPOCH);
            let desc = snapshot_to_field_desc(&snap);
            let value = snapshot_to_pv_field(&snap);
            crate::pvdata::value_matches_descriptor(&value, &desc)
                .unwrap_or_else(|e| panic!("value/desc mismatch for {v:?}: {e}"));
        }
    }

    /// `display.form.choices` carries the fixed seven-entry `Q:form` menu —
    /// `IOCSource::initialize` (`iocsource.cpp:39-65`). Measured on the C
    /// side for `ai`; the port sent an empty array.
    #[test]
    fn form_choices_carry_the_qform_menu() {
        let snap = Snapshot::new(EpicsValue::Double(0.0), 0, 0, std::time::UNIX_EPOCH);
        let PvField::Structure(root) = snapshot_to_pv_field(&snap) else {
            panic!("NT must be a structure");
        };
        let PvField::Structure(display) = root.get_field("display").expect("display") else {
            panic!("display must be a structure");
        };
        let PvField::Structure(form) = display.get_field("form").expect("form") else {
            panic!("form must be a structure");
        };
        let PvField::ScalarArray(choices) = form.get_field("choices").expect("choices") else {
            panic!("choices must be a scalar array");
        };
        let got: Vec<String> = choices
            .iter()
            .map(|c| match c {
                ScalarValue::String(s) => s.to_string(),
                other => panic!("choice not a string: {other:?}"),
            })
            .collect();
        assert_eq!(
            got,
            [
                "Default",
                "String",
                "Binary",
                "Decimal",
                "Hex",
                "Exponential",
                "Engineering"
            ],
        );
    }

    #[test]
    fn build_timestamp_uses_snapshot_acquisition_time_and_user_tag() {
        // pvxs `iocsource.cpp:240-248`: timeStamp carries the record's
        // acquisition time + userTag, not the serialization wall-clock.
        // Inject the exact (secs, nsec); a `SystemTime` rounds 123_456_789 to
        // 100 ns on Windows before `build_timestamp` ever reads it.
        let ts = WallTime::from_unix(1_700_000_000, 123_456_789);
        let mut snap = Snapshot::new(EpicsValue::Double(1.0), 0, 0, ts);
        snap.user_tag = 42;
        let PvField::Structure(t) = build_timestamp(&snap) else {
            panic!("timeStamp must be a structure");
        };
        assert!(matches!(
            scalar(&t, "secondsPastEpoch"),
            ScalarValue::Long(1_700_000_000)
        ));
        assert!(matches!(
            scalar(&t, "nanoseconds"),
            ScalarValue::Int(123_456_789)
        ));
        assert!(matches!(scalar(&t, "userTag"), ScalarValue::Int(42)));
    }

    #[test]
    fn build_value_alarm_uses_display_alarm_limits() {
        // pvxs `iocsource.cpp:300-303`: valueAlarm limits come from
        // DBR_AL_DOUBLE (DisplayInfo alarm/warning limits), not 0.0.
        use epics_base_rs::server::snapshot::DisplayInfo;
        let mut snap = Snapshot::new(EpicsValue::Double(1.0), 0, 0, std::time::UNIX_EPOCH);
        snap.display = Some(DisplayInfo {
            lower_alarm_limit: -10.0,
            lower_warning_limit: -5.0,
            upper_warning_limit: 5.0,
            upper_alarm_limit: 10.0,
            ..Default::default()
        });
        let PvField::Structure(root) = snapshot_to_pv_field(&snap) else {
            panic!("NT must be a structure");
        };
        let PvField::Structure(v) = root.get_field("valueAlarm").expect("valueAlarm") else {
            panic!("valueAlarm must be a structure");
        };
        let get = |name: &str| match scalar(v, name) {
            ScalarValue::Double(x) => x,
            other => panic!("{name} not Double: {other:?}"),
        };
        assert_eq!(get("lowAlarmLimit"), -10.0);
        assert_eq!(get("lowWarningLimit"), -5.0);
        assert_eq!(get("highWarningLimit"), 5.0);
        assert_eq!(get("highAlarmLimit"), 10.0);
    }

    #[test]
    fn build_display_emits_description() {
        // pvxs `iocsource.cpp:306-308`: display.description carries the
        // record DESC; in Rust it is DisplayInfo.description, not "".
        use epics_base_rs::server::snapshot::DisplayInfo;
        let mut snap = Snapshot::new(EpicsValue::Double(1.0), 0, 0, std::time::UNIX_EPOCH);
        snap.display = Some(DisplayInfo {
            description: "chamber pressure".into(),
            units: "Torr".into(),
            ..Default::default()
        });
        let PvField::Structure(root) = snapshot_to_pv_field(&snap) else {
            panic!("NT must be a structure");
        };
        let PvField::Structure(d) = root.get_field("display").expect("display") else {
            panic!("display must be a structure");
        };
        assert!(matches!(
            scalar(d, "description"),
            ScalarValue::String(s) if s == "chamber pressure"
        ));
        assert!(matches!(
            scalar(d, "units"),
            ScalarValue::String(s) if s == "Torr"
        ));
    }

    #[test]
    fn build_alarm_message_is_condition_string() {
        // pvxs `iocsource.cpp:226,236`: alarm.message is the condition
        // string for a non-zero status, "" for NO_ALARM.
        let mut snap = Snapshot::new(EpicsValue::Double(1.0), 0, 0, std::time::UNIX_EPOCH);
        snap.alarm.status = alarm_status::HIHI_ALARM;
        snap.alarm.severity = 2;
        let PvField::Structure(a) = build_alarm(&snap) else {
            panic!("alarm must be a structure");
        };
        assert!(matches!(
            scalar(&a, "message"),
            ScalarValue::String(s) if s == "HIHI"
        ));

        // NO_ALARM keeps an empty message.
        let ok = Snapshot::new(EpicsValue::Double(1.0), 0, 0, std::time::UNIX_EPOCH);
        let PvField::Structure(a) = build_alarm(&ok) else {
            panic!("alarm must be a structure");
        };
        assert!(matches!(
            scalar(&a, "message"),
            ScalarValue::String(s) if s.is_empty()
        ));
    }

    #[test]
    fn build_alarm_prefers_carried_amsg_over_condition_string() {
        // pvxs `iocsource.cpp:230-236`: a non-empty carried amsg wins over
        // the synthesized condition string. mbboDirect raises UDF with the
        // bespoke "UDFS" (`mbboDirectRecord.c:191`), so an undefined
        // mbboDirect serves alarm.message = "UDFS", NOT the "UDF" condition.
        let mut snap = Snapshot::new(EpicsValue::Enum(0), 0, 0, std::time::UNIX_EPOCH);
        snap.alarm.status = alarm_status::UDF_ALARM;
        snap.alarm.severity = 3;
        snap.alarm.amsg = "UDFS".to_string();
        let PvField::Structure(a) = build_alarm(&snap) else {
            panic!("alarm must be a structure");
        };
        assert!(
            matches!(scalar(&a, "message"), ScalarValue::String(s) if s == "UDFS"),
            "non-empty carried amsg must win over the UDF condition string"
        );

        // Empty amsg with a non-zero status falls back to the condition
        // string — how every non-mbboDirect UDF record (empty namsg from
        // plain recGblSetSevr) serves "UDF".
        let mut generic = Snapshot::new(EpicsValue::Double(0.0), 0, 0, std::time::UNIX_EPOCH);
        generic.alarm.status = alarm_status::UDF_ALARM;
        generic.alarm.severity = 3;
        assert!(generic.alarm.amsg.is_empty());
        let PvField::Structure(a) = build_alarm(&generic) else {
            panic!("alarm must be a structure");
        };
        assert!(
            matches!(scalar(&a, "message"), ScalarValue::String(s) if s == "UDF"),
            "empty amsg must fall back to the condition string"
        );

        // Empty amsg with NO_ALARM stays "".
        let ok = Snapshot::new(EpicsValue::Double(1.0), 0, 0, std::time::UNIX_EPOCH);
        let PvField::Structure(a) = build_alarm(&ok) else {
            panic!("alarm must be a structure");
        };
        assert!(matches!(scalar(&a, "message"), ScalarValue::String(s) if s.is_empty()));
    }

    #[test]
    fn build_alarm_status_is_pva_class_not_raw_condition() {
        // pvxs `iocsource.cpp:187-223`: emit the alarm *class* (0–6), not
        // the raw EPICS condition. A `LINK_ALARM = 14` must surface as
        // class 3 (RECORD), never 14.
        let mut snap = Snapshot::new(EpicsValue::Double(1.0), 0, 0, std::time::UNIX_EPOCH);
        snap.alarm.status = alarm_status::LINK_ALARM;
        let PvField::Structure(a) = build_alarm(&snap) else {
            panic!("alarm must be a structure");
        };
        assert!(
            matches!(scalar(&a, "status"), ScalarValue::Int(3)),
            "LINK_ALARM (14) must map to status class 3 (RECORD)"
        );

        // Whole condition→class table, one representative per class.
        assert_eq!(alarm_status_class(alarm_status::NO_ALARM), 0); // NONE
        assert_eq!(alarm_status_class(alarm_status::HIGH_ALARM), 1); // DEVICE
        assert_eq!(alarm_status_class(alarm_status::HW_LIMIT_ALARM), 1); // DEVICE
        assert_eq!(alarm_status_class(alarm_status::COMM_ALARM), 2); // DRIVER
        assert_eq!(alarm_status_class(alarm_status::UDF_ALARM), 2); // DRIVER
        assert_eq!(alarm_status_class(alarm_status::CALC_ALARM), 3); // RECORD
        assert_eq!(alarm_status_class(alarm_status::SCAN_ALARM), 3); // RECORD
        assert_eq!(alarm_status_class(alarm_status::DISABLE_ALARM), 4); // DB
        assert_eq!(alarm_status_class(alarm_status::WRITE_ACCESS_ALARM), 4); // DB
        // Out-of-range / unmapped → UNDEFINED.
        assert_eq!(alarm_status_class(99), 6);
    }

    #[test]
    fn enum_snapshot_builds_nt_enum_value_and_desc() {
        // pvxs `ioc/singlesource.cpp:200-201`: a DBR_ENUM scalar surfaces
        // as `epics:nt/NTEnum:1.0` with `value.index` + `value.choices`,
        // not a numeric NTScalar.
        use epics_base_rs::server::snapshot::EnumInfo;
        let mut snap = Snapshot::new(EpicsValue::Enum(1), 0, 0, std::time::UNIX_EPOCH);
        snap.enums = Some(EnumInfo::new(vec!["OFF".into(), "ON".into()]));

        // value: NTEnum struct id + nested enum_t { index, choices }.
        let PvField::Structure(s) = snapshot_to_pv_field(&snap) else {
            panic!("NTEnum value must be a structure");
        };
        assert_eq!(s.struct_id, "epics:nt/NTEnum:1.0");
        // pvxs NTEnum carries no control/valueAlarm.
        assert!(s.get_field("control").is_none(), "NTEnum has no control");
        assert!(
            s.get_field("valueAlarm").is_none(),
            "NTEnum has no valueAlarm"
        );
        let Some(PvField::Structure(value)) = s.get_field("value") else {
            panic!("NTEnum.value must be an enum_t structure");
        };
        assert_eq!(value.struct_id, "enum_t");
        assert!(matches!(
            value.get_field("index"),
            Some(PvField::Scalar(ScalarValue::Int(1)))
        ));
        let Some(PvField::ScalarArray(choices)) = value.get_field("choices") else {
            panic!("NTEnum.value.choices must be a scalar array");
        };
        assert_eq!(
            choices,
            &vec![
                ScalarValue::String("OFF".into()),
                ScalarValue::String("ON".into())
            ]
        );

        // descriptor must stay in lockstep with the value shape.
        let FieldDesc::Structure {
            struct_id, fields, ..
        } = snapshot_to_field_desc(&snap)
        else {
            panic!("NTEnum descriptor must be a structure");
        };
        assert_eq!(struct_id, "epics:nt/NTEnum:1.0");
        let value_desc = &fields.iter().find(|(n, _)| n == "value").unwrap().1;
        let FieldDesc::Structure {
            struct_id: vid,
            fields: vfields,
            ..
        } = value_desc
        else {
            panic!("NTEnum value descriptor must be a structure");
        };
        assert_eq!(vid, "enum_t");
        assert!(matches!(
            vfields.iter().find(|(n, _)| n == "index").unwrap().1,
            FieldDesc::Scalar(ScalarType::Int)
        ));
        assert!(matches!(
            vfields.iter().find(|(n, _)| n == "choices").unwrap().1,
            FieldDesc::ScalarArray(ScalarType::String)
        ));

        // An enum snapshot with no choice metadata yields empty choices,
        // not a panic or a missing field.
        let bare = Snapshot::new(EpicsValue::Enum(0), 0, 0, std::time::UNIX_EPOCH);
        let PvField::Structure(bs) = snapshot_to_pv_field(&bare) else {
            panic!("structure");
        };
        let Some(PvField::Structure(bv)) = bs.get_field("value") else {
            panic!("enum_t");
        };
        assert!(matches!(
            bv.get_field("choices"),
            Some(PvField::ScalarArray(c)) if c.is_empty()
        ));
    }

    /// `DBF_CHAR` is signed on the pvAccess wire: pvxs `fromDbrType` maps
    /// `DBR_CHAR -> TypeCode::Int8` and `DBR_UCHAR -> TypeCode::UInt8`
    /// (`ioc/typeutils.cpp:32-35`). A `Char` value of 200 (0xC8) must serve as
    /// the signed byte −56 with a `byte` descriptor, not unsigned 200/`ubyte`
    /// — while the unsigned `UChar` twin stays `ubyte`/200. Both the value and
    /// the descriptor path are covered so they cannot drift apart.
    #[test]
    fn dbf_char_serves_as_signed_byte_uchar_stays_unsigned() {
        let ts = std::time::UNIX_EPOCH;

        // Scalar DBF_CHAR → signed byte (value + descriptor).
        let snap = Snapshot::new(EpicsValue::Char(200), 0, 0, ts);
        let PvField::Structure(s) = snapshot_to_pv_field(&snap) else {
            panic!("NTScalar value must be a structure");
        };
        let Some(PvField::Scalar(ScalarValue::Byte(b))) = s.get_field("value") else {
            panic!(
                "DBF_CHAR value must be a signed byte scalar, got {:?}",
                s.get_field("value")
            );
        };
        assert_eq!(*b, -56, "DBF_CHAR 200 must serve as signed byte −56");
        let FieldDesc::Structure { fields, .. } = snapshot_to_field_desc(&snap) else {
            panic!("NTScalar descriptor must be a structure");
        };
        assert!(
            matches!(
                fields.iter().find(|(n, _)| n == "value").unwrap().1,
                FieldDesc::Scalar(ScalarType::Byte)
            ),
            "DBF_CHAR descriptor must be signed `byte`"
        );

        // Scalar DBF_UCHAR → unsigned byte (the untouched twin).
        let usnap = Snapshot::new(EpicsValue::UChar(200), 0, 0, ts);
        let PvField::Structure(us) = snapshot_to_pv_field(&usnap) else {
            panic!("structure");
        };
        let Some(PvField::Scalar(ScalarValue::UByte(u))) = us.get_field("value") else {
            panic!("DBF_UCHAR value must be an unsigned byte scalar");
        };
        assert_eq!(*u, 200, "DBF_UCHAR must stay unsigned 200");

        // Array DBF_CHAR[] → signed byte[], element-wise (value + descriptor).
        let asnap = Snapshot::new(EpicsValue::CharArray(vec![200, 0, 127]), 0, 0, ts);
        let PvField::Structure(a) = snapshot_to_pv_field(&asnap) else {
            panic!("structure");
        };
        let Some(PvField::ScalarArray(arr)) = a.get_field("value") else {
            panic!("DBF_CHAR[] value must be a scalar array");
        };
        assert_eq!(
            arr,
            &vec![
                ScalarValue::Byte(-56),
                ScalarValue::Byte(0),
                ScalarValue::Byte(127),
            ],
            "DBF_CHAR[] must serve as signed byte[]"
        );
        let FieldDesc::Structure {
            fields: afields, ..
        } = snapshot_to_field_desc(&asnap)
        else {
            panic!("structure");
        };
        assert!(
            matches!(
                afields.iter().find(|(n, _)| n == "value").unwrap().1,
                FieldDesc::ScalarArray(ScalarType::Byte)
            ),
            "DBF_CHAR[] descriptor must be signed `byte[]`"
        );
    }

    #[test]
    fn non_utf8_units_and_enum_choices_survive_pva_metadata_encoder() {
        // pvxs serialises wire strings verbatim (`pvaproto.h:403`). EPICS
        // EGU (`display.units`) and enum state labels (`value.choices`) are
        // raw record bytes with no UTF-8 guarantee, so a non-UTF-8 EGU or
        // choice must reach the wire unmangled. The metadata boundary
        // (DisplayInfo.units / EnumInfo.strings) must be byte-preserving,
        // not a lossy `String` round-trip.
        use crate::proto::buffer::ByteOrder;
        use crate::pvdata::encode::{decode_scalar_value, encode_scalar_value};
        use epics_base_rs::server::snapshot::{DisplayInfo, EnumInfo};
        use std::io::Cursor;

        // 0xFF/0x00/0xFE/0x80 make this byte sequence invalid UTF-8, so a
        // `String` conversion anywhere on the path would mangle it.
        let raw = vec![0xFFu8, 0x00, 0xFE, b'd', b'e', b'g', 0x80];

        // Encode a String scalar to the wire and decode it back.
        let round_trip = |sv: &ScalarValue| -> ScalarValue {
            let mut buf = Vec::new();
            encode_scalar_value(sv, ByteOrder::Little, &mut buf);
            let mut cur = Cursor::new(buf.as_slice());
            decode_scalar_value(ScalarType::String, &mut cur, ByteOrder::Little).unwrap()
        };

        // display.units boundary: producer struct, then wire round-trip.
        let mut snap = Snapshot::new(EpicsValue::Double(1.0), 0, 0, std::time::UNIX_EPOCH);
        snap.display = Some(DisplayInfo {
            units: PvString::from_bytes(raw.clone()),
            ..Default::default()
        });
        let PvField::Structure(root) = snapshot_to_pv_field(&snap) else {
            panic!("NT must be a structure");
        };
        let PvField::Structure(d) = root.get_field("display").expect("display") else {
            panic!("display must be a structure");
        };
        let ScalarValue::String(units) = scalar(d, "units") else {
            panic!("units must be a string scalar");
        };
        assert_eq!(
            units.as_bytes(),
            raw.as_slice(),
            "units bytes lost at the producer->struct boundary"
        );
        let ScalarValue::String(units_wire) = round_trip(&ScalarValue::String(units.clone()))
        else {
            panic!("decoded units must stay a string");
        };
        assert_eq!(
            units_wire.as_bytes(),
            raw.as_slice(),
            "units bytes mangled by the wire encoder"
        );

        // value.choices boundary: producer struct, then wire round-trip.
        let mut esnap = Snapshot::new(EpicsValue::Enum(0), 0, 0, std::time::UNIX_EPOCH);
        esnap.enums = Some(EnumInfo::new(vec![PvString::from_bytes(raw.clone())]));
        let PvField::Structure(s) = snapshot_to_pv_field(&esnap) else {
            panic!("NTEnum value must be a structure");
        };
        let Some(PvField::Structure(value)) = s.get_field("value") else {
            panic!("NTEnum.value must be a structure");
        };
        let Some(PvField::ScalarArray(choices)) = value.get_field("choices") else {
            panic!("choices must be a scalar array");
        };
        let ScalarValue::String(choice) = &choices[0] else {
            panic!("choice must be a string scalar");
        };
        assert_eq!(
            choice.as_bytes(),
            raw.as_slice(),
            "enum choice bytes lost at the producer->struct boundary"
        );
        let ScalarValue::String(choice_wire) = round_trip(&ScalarValue::String(choice.clone()))
        else {
            panic!("decoded choice must stay a string");
        };
        assert_eq!(
            choice_wire.as_bytes(),
            raw.as_slice(),
            "enum choice bytes mangled by the wire encoder"
        );
    }

    #[epics_macros_rs::epics_test]
    async fn put_nt_enum_dereferences_value_index() {
        // pvxs `ioc/iocsource.cpp:589-593`: an NTEnum PUT is dereferenced
        // through `value.index`. A round trip through the native source
        // must land the index on the backing enum record.
        use epics_base_rs::server::records::mbbi::MbbiRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("ENUM:PV", Box::new(MbbiRecord::new(0)))
            .await
            .unwrap();
        let src = PvDatabaseSource::new(db.clone());

        // NTEnum-shaped PUT carrying value.index = 1.
        let mut value = PvStructure::new("enum_t");
        value
            .fields
            .push(("index".into(), PvField::Scalar(ScalarValue::Int(1))));
        let mut nt = PvStructure::new("epics:nt/NTEnum:1.0");
        nt.fields.push(("value".into(), PvField::Structure(value)));

        src.put_value_ctx(
            "ENUM:PV",
            PvField::Structure(nt),
            make_ctx("127.0.0.1", "anon", "ca"),
        )
        .await
        .expect("NTEnum PUT must succeed");

        // Read back: value.index must reflect the put.
        let got = src
            .get_value_ctx("ENUM:PV", make_ctx("127.0.0.1", "anon", "ca"))
            .await
            .expect("GET must return a value");
        let PvField::Structure(s) = got else {
            panic!("expected NTEnum structure");
        };
        let Some(PvField::Structure(v)) = s.get_field("value") else {
            panic!("expected enum_t value");
        };
        assert!(
            matches!(
                v.get_field("index"),
                Some(PvField::Scalar(ScalarValue::Int(1)))
            ),
            "NTEnum PUT must land on value.index"
        );
    }

    /// The search/create gates by boundary, one case per boundary rather than
    /// per scenario. `searchable` is the dbd-name question (pvxs
    /// `dbChannelTest`); `has_pv` is the can-this-be-served question (pvxs
    /// `getValuePrototype`). The two answers must differ exactly on a name
    /// whose record exists but whose field cannot be served.
    ///
    /// Measured against `softIocPVX` on `record(ai,"ORACLE:AI")`:
    /// `pvxget ORACLE:AI.MLOK` → `Refused to create Channel`, i.e. the search
    /// WAS answered (a refusal can only follow a CREATE_CHANNEL, which only
    /// follows a successful search) and the create was refused.
    #[epics_macros_rs::epics_test]
    async fn search_claims_every_dbd_name_but_create_gates_on_a_servable_field() {
        use epics_base_rs::server::records::ai::AiRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("ORACLE:AI", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_pv("SIMPLE:PV", EpicsValue::Double(1.0))
            .await
            .unwrap();
        let src = PvDatabaseSource::new(db.clone());

        // Boundary: record base name — servable (VAL), both gates true.
        assert!(src.searchable("ORACLE:AI").await);
        assert!(src.has_pv("ORACLE:AI").await);

        // Boundary: an explicitly addressed servable field.
        assert!(src.searchable("ORACLE:AI.EGU").await);
        assert!(src.has_pv("ORACLE:AI.EGU").await);

        // Boundary: THE case. `MLOK` is `DBF_NOACCESS` in ai.dbd (a mutex, so
        // this port models no field for it). The record resolves, so search
        // must still answer; the field has no value, so create must refuse.
        // This is the one name where the two gates disagree.
        assert!(
            src.searchable("ORACLE:AI.MLOK").await,
            "a DBF_NOACCESS field's record resolves, so pvxs answers the \
             SEARCH — withholding it would replace `Refused to create \
             Channel` with a client timeout"
        );
        assert!(
            !src.has_pv("ORACLE:AI.MLOK").await,
            "a DBF_NOACCESS field has no NT, so CREATE_CHANNEL must be refused \
             rather than claimed and later failed with `field introspection \
             unavailable`"
        );

        // Boundary: a record that does not exist at all — neither gate.
        assert!(!src.searchable("NO:SUCH:RECORD").await);
        assert!(!src.has_pv("NO:SUCH:RECORD").await);

        // Boundary: a simple PV has no field to narrow, so the stricter create
        // gate must not change its answer.
        assert!(src.searchable("SIMPLE:PV").await);
        assert!(src.has_pv("SIMPLE:PV").await);
    }

    /// Regression: a monitor on a *simple* native PV must observe later
    /// PUTs, not just the connect-time snapshot. Pre-fix the `Simple` arm
    /// sent one snapshot and dropped the channel, so a PVA PUT through the
    /// same server never reached the monitor. pvxs `SharedPV::post()` fans
    /// every update out to its stored subscribers (`sharedpv.cpp:417-440`).
    /// The `value` leaf of a monitor frame, whether the source framed a bare
    /// scalar or a full NT structure.
    fn monitor_double(field: &PvField) -> Option<f64> {
        match field {
            PvField::Scalar(ScalarValue::Double(v)) => Some(*v),
            PvField::Structure(s) => match s.get_field("value") {
                Some(PvField::Scalar(ScalarValue::Double(v))) => Some(*v),
                _ => None,
            },
            _ => None,
        }
    }

    #[epics_macros_rs::epics_test]
    async fn simple_pv_monitor_observes_later_puts() {
        let db = Arc::new(PvDatabase::new());
        db.add_pv("SIMPLE:MON", EpicsValue::Double(1.0))
            .await
            .unwrap();
        let src = PvDatabaseSource::new(db.clone());

        let mut rx = src
            .subscribe_ctx("SIMPLE:MON", make_ctx("127.0.0.1", "anon", "ca"))
            .await
            .expect("subscribe to simple PV");

        // Connect-time snapshot.
        let first = rx.recv().await.expect("initial monitor snapshot");
        assert_eq!(monitor_double(&first), Some(1.0));

        // A PVA PUT through the same source must reach the monitor.
        src.put_value_ctx(
            "SIMPLE:MON",
            pv_double(2.0),
            make_ctx("127.0.0.1", "anon", "ca"),
        )
        .await
        .expect("PUT must succeed");

        let updated =
            epics_base_rs::runtime::task::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("monitor must receive the PUT update within 2s")
                .expect("monitor stream still open");
        assert_eq!(
            monitor_double(&updated),
            Some(2.0),
            "simple-PV monitor must observe the PUT, not only the initial snapshot"
        );
    }

    /// Regression: PVA PUT must be gated through ACF when
    /// the source was built with ACF enforcement. Pre-fix the PVA
    /// server stored ACF only as `#[allow(dead_code)]` and never
    /// called `check_access*` — every client could PUT regardless
    /// of UAG/HAG/RULE configuration.
    #[epics_macros_rs::epics_test]
    async fn put_value_ctx_denies_when_acf_writeable_rule_unmet() {
        use epics_base_rs::server::records::ai::AiRecord;

        // ACF: only `admin` user on `host:lab` may WRITE.
        let acf_text = r#"
UAG(admins) { admin }
HAG(lab) { lab-pc1 }
ASG(SECURE) {
    RULE(1, READ)
    RULE(1, WRITE) { UAG(admins) HAG(lab) }
}
"#;
        let acf = parse_acf(acf_text).unwrap();

        let db = Arc::new(PvDatabase::new());
        db.add_record("AI:SECURE", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        // Mark the record as belonging to the SECURE ASG.
        let rec = db.get_record("AI:SECURE").unwrap();
        rec.write().common.asg = "SECURE".to_string();

        let source = PvDatabaseSource::new_with_acf(
            db.clone(),
            epics_base_rs::server::access_security::new_acf_cell(Some(acf)),
        );

        // Allowed: admin from lab-pc1 — must succeed.
        source
            .put_value_ctx(
                "AI:SECURE",
                pv_double(1.0),
                make_ctx("lab-pc1", "admin", "anonymous"),
            )
            .await
            .expect("admin/lab-pc1 must be allowed to PUT");

        // Denied: regular user from a non-lab host — must fail with
        // a recognisable error.
        let err = source
            .put_value_ctx(
                "AI:SECURE",
                pv_double(2.0),
                make_ctx("intruder-pc", "guest", "anonymous"),
            )
            .await
            .expect_err("non-admin must be denied");
        assert!(
            err.contains("denied by access security"),
            "denial reason must be visible: {err:?}",
        );
    }

    /// Regression: ACF must also gate READ. A peer with
    /// NoAccess on the record's ASG must observe the same shape as
    /// "PV not found" (None) — the wire layer surfaces this as
    /// ECA_NORDACCESS-equivalent.
    #[epics_macros_rs::epics_test]
    async fn get_value_ctx_denies_when_acf_no_access() {
        use epics_base_rs::server::records::ai::AiRecord;

        // ACF: only members of `ops` UAG may READ, denying everyone
        // else (no fall-through RULE).
        let acf_text = r#"
UAG(ops) { alice }
ASG(LOCKED) {
    RULE(1, READ) { UAG(ops) }
}
"#;
        let acf = parse_acf(acf_text).unwrap();

        let db = Arc::new(PvDatabase::new());
        db.add_record("AI:LOCKED", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let rec = db.get_record("AI:LOCKED").unwrap();
        rec.write().common.asg = "LOCKED".to_string();

        let source = PvDatabaseSource::new_with_acf(
            db.clone(),
            epics_base_rs::server::access_security::new_acf_cell(Some(acf)),
        );

        // alice gets a value back.
        let v = source
            .get_value_ctx("AI:LOCKED", make_ctx("h", "alice", "anonymous"))
            .await;
        assert!(v.is_some(), "alice must be allowed to read");

        // intruder gets None — same shape as unknown PV, surfaced as
        // an access-denied at the wire layer.
        let v = source
            .get_value_ctx("AI:LOCKED", make_ctx("h", "intruder", "anonymous"))
            .await;
        assert!(v.is_none(), "intruder must be denied at READ time");
    }

    /// subscribe_ctx must also deny when peer has
    /// NoAccess.
    #[epics_macros_rs::epics_test]
    async fn subscribe_ctx_denies_when_acf_no_access() {
        use epics_base_rs::server::records::ai::AiRecord;

        let acf_text = r#"
UAG(ops) { alice }
ASG(LOCKED) {
    RULE(1, READ) { UAG(ops) }
}
"#;
        let acf = parse_acf(acf_text).unwrap();

        let db = Arc::new(PvDatabase::new());
        db.add_record("AI:MON", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let rec = db.get_record("AI:MON").unwrap();
        rec.write().common.asg = "LOCKED".to_string();

        let source = PvDatabaseSource::new_with_acf(
            db.clone(),
            epics_base_rs::server::access_security::new_acf_cell(Some(acf)),
        );

        let rx = source
            .subscribe_ctx("AI:MON", make_ctx("h", "alice", "anonymous"))
            .await;
        assert!(rx.is_some(), "alice MONITOR must be allowed");

        let rx = source
            .subscribe_ctx("AI:MON", make_ctx("h", "intruder", "anonymous"))
            .await;
        assert!(rx.is_none(), "intruder MONITOR must be denied");
    }

    /// Regression: AcfCell swap takes effect on the next
    /// ACF check. A subsequent PUT after the swap must use the new
    /// policy. Proves the RwLock-backed reload path actually
    /// influences the source's behaviour (the reload path plumbs
    /// `PvaServer::reload_acf_from` to write into this cell).
    #[epics_macros_rs::epics_test]
    async fn acf_swap_takes_effect_on_next_put() {
        use epics_base_rs::server::records::ai::AiRecord;

        // Initial: deny everyone.
        let lockdown = parse_acf(
            r#"
ASG(SECURE) {
    RULE(1, READ)
}
"#,
        )
        .unwrap();
        let cell: AcfCell = epics_base_rs::server::access_security::new_acf_cell(Some(lockdown));

        let db = Arc::new(PvDatabase::new());
        db.add_record("AI:LIVE", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let rec = db.get_record("AI:LIVE").unwrap();
        rec.write().common.asg = "SECURE".to_string();

        let source = PvDatabaseSource::new_with_acf(db.clone(), cell.clone());

        // First put: denied (READ-only ASG).
        assert!(
            source
                .put_value_ctx(
                    "AI:LIVE",
                    pv_double(1.0),
                    make_ctx("h", "anyone", "anonymous"),
                )
                .await
                .is_err(),
            "initial policy must deny PUT"
        );

        // Hot-swap: open up WRITE for everyone.
        let permissive = parse_acf(
            r#"
ASG(SECURE) {
    RULE(1, READ)
    RULE(1, WRITE)
}
"#,
        )
        .unwrap();
        cell.store(Some(Arc::new(permissive)));

        // Second put: succeeds under the new policy.
        source
            .put_value_ctx(
                "AI:LIVE",
                pv_double(2.0),
                make_ctx("h", "anyone", "anonymous"),
            )
            .await
            .expect("post-swap policy must allow PUT");
    }

    /// When the source is built without ACF, behaviour
    /// matches the old `put_value` path — every PUT succeeds.
    #[epics_macros_rs::epics_test]
    async fn put_value_ctx_allows_when_no_acf() {
        use epics_base_rs::server::records::ai::AiRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("AI:OPEN", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();

        let source = PvDatabaseSource::new(db.clone());
        source
            .put_value_ctx(
                "AI:OPEN",
                pv_double(7.0),
                make_ctx("anywhere", "anyone", "anonymous"),
            )
            .await
            .expect("PUT must succeed when no ACF is attached");
    }

    /// Type-state-gated GET. The wire-layer flow mints an
    /// [`AccessChecked`] via `source.access().check(...)`; the source
    /// then sees `NoAccess` in the token level and returns `None`.
    /// Previously every GET handler had to *remember* to call the
    /// ACF check by hand — now the trait method signature forces it.
    #[epics_macros_rs::epics_test]
    async fn get_value_checked_denies_when_no_access() {
        use crate::server_native::source::ChannelSource;
        use epics_base_rs::server::records::ai::AiRecord;

        let acf = parse_acf(
            r#"
UAG(ops) { alice }
ASG(LOCKED) {
    RULE(0, READ) { UAG(ops) }
}
"#,
        )
        .unwrap();

        let db = Arc::new(PvDatabase::new());
        db.add_record("AI:LOCKED", Box::new(AiRecord::new(7.5)))
            .await
            .unwrap();
        db.get_record("AI:LOCKED").unwrap().write().common.asg = "LOCKED".into();

        let source = PvDatabaseSource::new_with_acf(
            db.clone(),
            epics_base_rs::server::access_security::new_acf_cell(Some(acf)),
        );

        // Allowed peer: gate mints a ReadWrite/Read token → source
        // returns the value.
        let checked_ok = source
            .access()
            .check("AI:LOCKED", "anyhost", "alice", "anonymous", "")
            .await;
        let val = source
            .get_value_checked(checked_ok, make_ctx("anyhost", "alice", "anonymous"))
            .await;
        assert!(val.is_some(), "alice must see the value");

        // Denied peer: gate mints a NoAccess token → source returns
        // None even though the underlying PV exists.
        let checked_deny = source
            .access()
            .check("AI:LOCKED", "anyhost", "intruder", "anonymous", "")
            .await;
        let val = source
            .get_value_checked(checked_deny, make_ctx("anyhost", "intruder", "anonymous"))
            .await;
        assert!(val.is_none(), "intruder must be denied via type-state gate");
    }

    /// Regression: a native PVA scalar `ulong` PUT into a
    /// `DBF_UINT64`-backed PV must preserve the full unsigned 64-bit
    /// range. Pre-fix `scalar_to_epics` collapsed `ScalarValue::ULong`
    /// to `EpicsValue::Long(x as i32)`, discarding the upper 32 bits
    /// before `PvDatabase::put_pv` coerced to the target field.
    #[epics_macros_rs::epics_test]
    async fn mr_r21_scalar_ulong_put_preserves_full_u64() {
        let db = Arc::new(PvDatabase::new());
        // A simple PV stores the value verbatim (`pv.set`), with no
        // field-type coercion — so it isolates the source-layer
        // conversion under test.
        db.add_pv("UL:SCALAR", EpicsValue::UInt64(0)).await.unwrap();

        let source = PvDatabaseSource::new(db.clone());

        // A value above the signed 32-bit range that also exercises
        // the high word: 0xDEAD_BEEF_0000_0001.
        let big: u64 = 0xDEAD_BEEF_0000_0001;
        source
            .put_value_ctx(
                "UL:SCALAR",
                PvField::Scalar(ScalarValue::ULong(big)),
                make_ctx("h", "anyone", "anonymous"),
            )
            .await
            .expect("ulong PUT must succeed");

        let snap = snapshot_for(&db, "UL:SCALAR").await.unwrap();
        assert_eq!(
            snap.value,
            EpicsValue::UInt64(big),
            "scalar ulong PUT must round-trip the full u64, got {:?}",
            snap.value,
        );
    }

    /// Regression: a native PVA `ulong[]` PUT into a
    /// `DBF_UINT64`-backed waveform must round-trip. Pre-fix
    /// `pv_field_to_epics` had no `ScalarValue::ULong` array arm, so
    /// the PUT fell through to `None` and was rejected as
    /// "PUT value not representable as EpicsValue".
    #[epics_macros_rs::epics_test]
    async fn mr_r24_ulong_array_put_preserves_full_u64() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;

        let db = Arc::new(PvDatabase::new());
        db.add_record(
            "UL:WF",
            Box::new(WaveformRecord::new(4, DbFieldType::UInt64)),
        )
        .await
        .unwrap();

        let source = PvDatabaseSource::new(db.clone());

        // Two of the four elements exceed i64::MAX, so any narrowing
        // to a signed type would corrupt them.
        let values: Vec<u64> = vec![1, 0xDEAD_BEEF_0000_0001, u64::MAX, 0];
        let put = PvField::ScalarArray(values.iter().map(|v| ScalarValue::ULong(*v)).collect());
        source
            .put_value_ctx("UL:WF", put, make_ctx("h", "anyone", "anonymous"))
            .await
            .expect("ulong[] PUT must succeed");

        let snap = snapshot_for(&db, "UL:WF").await.unwrap();
        assert_eq!(
            snap.value,
            EpicsValue::UInt64Array(values),
            "ulong[] PUT must round-trip the full u64 elements, got {:?}",
            snap.value,
        );
    }

    /// a real PVA wire `ulong[]` PUT decodes to
    /// `PvField::ScalarArrayTyped`, not the hand-built untyped
    /// `ScalarArray` that `mr_r24` used. Before the fix
    /// `pv_field_to_epics` had only a `ScalarArray` arm, so the
    /// wire-decoded form fell through to `None` and the PUT was
    /// rejected as "PUT value not representable as EpicsValue".
    #[epics_macros_rs::epics_test]
    async fn pf_r2_wire_typed_ulong_array_put_preserves_full_u64() {
        use crate::pvdata::TypedScalarArray;
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;

        let db = Arc::new(PvDatabase::new());
        db.add_record(
            "UL:WFT",
            Box::new(WaveformRecord::new(4, DbFieldType::UInt64)),
        )
        .await
        .unwrap();
        let source = PvDatabaseSource::new(db.clone());

        let values: Vec<u64> = vec![1, 0xDEAD_BEEF_0000_0001, u64::MAX, 0];
        // Wire-decoded shape: `decode_pv_field` produces the typed,
        // refcount-shared array — not `PvField::ScalarArray`.
        let put = PvField::ScalarArrayTyped(TypedScalarArray::ULong(Arc::from(values.as_slice())));
        source
            .put_value_ctx("UL:WFT", put, make_ctx("h", "anyone", "anonymous"))
            .await
            .expect("wire-decoded ulong[] PUT must succeed");

        let snap = snapshot_for(&db, "UL:WFT").await.unwrap();
        assert_eq!(
            snap.value,
            EpicsValue::UInt64Array(values),
            "wire-decoded ulong[] PUT must round-trip the full u64 elements, got {:?}",
            snap.value,
        );
    }

    /// Regression: a native PVA scalar `uint` PUT above `i32::MAX`
    /// must not sign-flip. Pre-fix `scalar_to_epics` mapped
    /// `ScalarValue::UInt(x)` to `EpicsValue::Long(x as i32)`, so
    /// `0x8000_0000..=0xffff_ffff` was stored as a negative value.
    /// `Int64` is the lossless `epicsUInt32` carrier.
    #[epics_macros_rs::epics_test]
    async fn pva_03_scalar_uint_put_preserves_full_u32() {
        let db = Arc::new(PvDatabase::new());
        // A simple PV stores the value verbatim, isolating the
        // source-layer conversion under test (no field coercion).
        db.add_pv("UI:SCALAR", EpicsValue::Int64(0)).await.unwrap();

        let source = PvDatabaseSource::new(db.clone());

        // Above signed-32 range: 0x8000_0001 = 2_147_483_649.
        let big: u32 = 0x8000_0001;
        source
            .put_value_ctx(
                "UI:SCALAR",
                PvField::Scalar(ScalarValue::UInt(big)),
                make_ctx("h", "anyone", "anonymous"),
            )
            .await
            .expect("uint PUT must succeed");

        let snap = snapshot_for(&db, "UI:SCALAR").await.unwrap();
        assert_eq!(
            snap.value,
            EpicsValue::Int64(i64::from(big)),
            "scalar uint PUT must carry the full epicsUInt32 range, got {:?}",
            snap.value,
        );
    }

    /// Regression: a native PVA `uint[]` PUT was rejected outright —
    /// `scalar_array_to_epics` had no `ScalarValue::UInt` arm, so the
    /// PUT fell through to `None` ("PUT value not representable as
    /// EpicsValue"). `Int64Array` carries every `epicsUInt32` element.
    #[epics_macros_rs::epics_test]
    async fn pva_03_uint_array_put_preserves_full_u32() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;

        let db = Arc::new(PvDatabase::new());
        db.add_record(
            "UI:WF",
            Box::new(WaveformRecord::new(4, DbFieldType::Int64)),
        )
        .await
        .unwrap();

        let source = PvDatabaseSource::new(db.clone());

        // Two elements exceed i32::MAX; any cast through i32 corrupts them.
        let values: Vec<u32> = vec![1, 0x8000_0001, u32::MAX, 0];
        let put = PvField::ScalarArray(values.iter().map(|v| ScalarValue::UInt(*v)).collect());
        source
            .put_value_ctx("UI:WF", put, make_ctx("h", "anyone", "anonymous"))
            .await
            .expect("uint[] PUT must succeed");

        let snap = snapshot_for(&db, "UI:WF").await.unwrap();
        assert_eq!(
            snap.value,
            EpicsValue::Int64Array(values.iter().map(|v| i64::from(*v)).collect()),
            "uint[] PUT must round-trip the full epicsUInt32 elements, got {:?}",
            snap.value,
        );
    }

    /// a real PVA wire `uint[]` PUT decodes to
    /// `PvField::ScalarArrayTyped`; it must reach the same `Int64Array`
    /// conversion as the untyped form rather than being rejected.
    #[epics_macros_rs::epics_test]
    async fn pva_03_wire_typed_uint_array_put_preserves_full_u32() {
        use crate::pvdata::TypedScalarArray;
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;

        let db = Arc::new(PvDatabase::new());
        db.add_record(
            "UI:WFT",
            Box::new(WaveformRecord::new(4, DbFieldType::Int64)),
        )
        .await
        .unwrap();
        let source = PvDatabaseSource::new(db.clone());

        let values: Vec<u32> = vec![1, 0x8000_0001, u32::MAX, 0];
        let put = PvField::ScalarArrayTyped(TypedScalarArray::UInt(Arc::from(values.as_slice())));
        source
            .put_value_ctx("UI:WFT", put, make_ctx("h", "anyone", "anonymous"))
            .await
            .expect("wire-decoded uint[] PUT must succeed");

        let snap = snapshot_for(&db, "UI:WFT").await.unwrap();
        assert_eq!(
            snap.value,
            EpicsValue::Int64Array(values.iter().map(|v| i64::from(*v)).collect()),
            "wire-decoded uint[] PUT must round-trip the full epicsUInt32 elements, got {:?}",
            snap.value,
        );
    }

    /// A native PVA
    /// scalar `uint` PUT into a signed `DBF_LONG` field must truncate
    /// (C/pvxs `static_cast`: `uint32 0xffffffff -> int32 -1`,
    /// `testdata.cpp:596`), not saturate. The `uint` carrier is `Int64`;
    /// pre-fix `convert_to(DBF_LONG)` round-tripped through f64 and clamped
    /// `4294967295.0` to `i32::MAX` (2147483647) instead of yielding -1.
    #[epics_macros_rs::epics_test]
    async fn r0604_uint_scalar_put_truncates_into_signed_longout() {
        use epics_base_rs::server::records::longout::LongoutRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("UI:LO", Box::new(LongoutRecord::new(0)))
            .await
            .unwrap();
        let source = PvDatabaseSource::new(db.clone());

        source
            .put_value_ctx(
                "UI:LO",
                PvField::Scalar(ScalarValue::UInt(0xffff_ffff)),
                make_ctx("h", "anyone", "anonymous"),
            )
            .await
            .expect("uint PUT must succeed");

        let snap = snapshot_for(&db, "UI:LO").await.unwrap();
        assert_eq!(
            snap.value,
            EpicsValue::Long(-1),
            "uint 0xffffffff into DBF_LONG must truncate to -1, got {:?}",
            snap.value,
        );
    }

    /// A native PVA
    /// `uint[]` PUT into a signed `waveform(FTVL=LONG)` must truncate each
    /// element (pvxs `convertCast` `Dest(S[i])`, `sharedarray.cpp:160-166`).
    /// `{1, 2, 0xffffffff}` lands as `{1, 2, -1}`; pre-fix the last element
    /// saturated to `i32::MAX` through the f64 round-trip.
    #[epics_macros_rs::epics_test]
    async fn r0604_uint_array_put_truncates_into_signed_long_waveform() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;

        let db = Arc::new(PvDatabase::new());
        db.add_record(
            "UI:WFL",
            Box::new(WaveformRecord::new(3, DbFieldType::Long)),
        )
        .await
        .unwrap();
        let source = PvDatabaseSource::new(db.clone());

        let values: Vec<u32> = vec![1, 2, 0xffff_ffff];
        let put = PvField::ScalarArray(values.iter().map(|v| ScalarValue::UInt(*v)).collect());
        source
            .put_value_ctx("UI:WFL", put, make_ctx("h", "anyone", "anonymous"))
            .await
            .expect("uint[] PUT must succeed");

        let snap = snapshot_for(&db, "UI:WFL").await.unwrap();
        assert_eq!(
            snap.value,
            EpicsValue::LongArray(vec![1, 2, -1]),
            "uint[] {{1,2,0xffffffff}} into FTVL=LONG must truncate to {{1,2,-1}}, got {:?}",
            snap.value,
        );
    }

    /// PVA-04 regression: a wire-decoded *empty* typed scalar array must
    /// PUT successfully and clear the waveform. Pre-fix the typed array
    /// was erased to `Vec<ScalarValue>` and its element type recovered
    /// from `items[0]`; an empty slice has no first element, so the PUT
    /// was rejected before the database could set NORD to zero. The
    /// element type now comes from the `TypedScalarArray` tag, which is
    /// present at length zero. Covers `double[]`, `string[]`, and an
    /// integer array per the finding.
    #[epics_macros_rs::epics_test]
    async fn pva_04_empty_typed_double_array_put_clears_waveform() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;

        let db = Arc::new(PvDatabase::new());
        db.add_record(
            "EMPTY:DBL",
            Box::new(WaveformRecord::new(4, DbFieldType::Double)),
        )
        .await
        .unwrap();
        let source = PvDatabaseSource::new(db.clone());

        let put = PvField::ScalarArrayTyped(TypedScalarArray::Double(Arc::from([] as [f64; 0])));
        source
            .put_value_ctx("EMPTY:DBL", put, make_ctx("h", "anyone", "anonymous"))
            .await
            .expect("empty double[] PUT must be accepted, not rejected");

        let snap = snapshot_for(&db, "EMPTY:DBL").await.unwrap();
        assert_eq!(
            snap.value,
            EpicsValue::DoubleArray(vec![]),
            "empty double[] PUT must clear the waveform to zero elements, got {:?}",
            snap.value,
        );
    }

    #[epics_macros_rs::epics_test]
    async fn pva_04_empty_typed_string_array_put_accepted() {
        // `WaveformRecord` has no DBF_STRING storage, so a string array
        // lives on a simple PV, which stores the value verbatim and thus
        // isolates the source-layer typed-array conversion under test.
        let db = Arc::new(PvDatabase::new());
        db.add_pv(
            "EMPTY:STR",
            EpicsValue::StringArray(vec!["seed".into(), "other".into()]),
        )
        .await
        .unwrap();
        let source = PvDatabaseSource::new(db.clone());

        let put =
            PvField::ScalarArrayTyped(TypedScalarArray::String(Arc::from([] as [PvString; 0])));
        source
            .put_value_ctx("EMPTY:STR", put, make_ctx("h", "anyone", "anonymous"))
            .await
            .expect("empty string[] PUT must be accepted, not rejected");

        let snap = snapshot_for(&db, "EMPTY:STR").await.unwrap();
        assert_eq!(
            snap.value,
            EpicsValue::StringArray(vec![]),
            "empty string[] PUT must store zero string elements, got {:?}",
            snap.value,
        );
    }

    #[epics_macros_rs::epics_test]
    async fn pva_04_empty_typed_int_array_put_clears_waveform() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::DbFieldType;

        let db = Arc::new(PvDatabase::new());
        db.add_record(
            "EMPTY:INT",
            Box::new(WaveformRecord::new(4, DbFieldType::Long)),
        )
        .await
        .unwrap();
        let source = PvDatabaseSource::new(db.clone());

        // PVA `int` (i32) → `EpicsValue::LongArray` (the i32 array family).
        let put = PvField::ScalarArrayTyped(TypedScalarArray::Int(Arc::from([] as [i32; 0])));
        source
            .put_value_ctx("EMPTY:INT", put, make_ctx("h", "anyone", "anonymous"))
            .await
            .expect("empty int[] PUT must be accepted, not rejected");

        let snap = snapshot_for(&db, "EMPTY:INT").await.unwrap();
        assert_eq!(
            snap.value,
            EpicsValue::LongArray(vec![]),
            "empty int[] PUT must clear the waveform to zero elements, got {:?}",
            snap.value,
        );
    }

    /// Regression: a full NTScalar PUT with explicit alarm + timeStamp
    /// into a simple PV must round-trip those fields on a later GET.
    /// Pre-fix `put_value` extracted only the `value` leaf and dropped
    /// alarm/timeStamp, so a later GET rebuilt them from local defaults
    /// (NO_ALARM + wall-clock-now). pvxs mailbox SharedPV assigns the
    /// whole posted value (`sharedpv.cpp:417-432`).
    #[epics_macros_rs::epics_test]
    async fn pva_60_simple_pv_put_persists_alarm_and_timestamp() {
        let db = Arc::new(PvDatabase::new());
        db.add_pv("MB:PV", EpicsValue::Double(0.0)).await.unwrap();
        let source = PvDatabaseSource::new(db.clone());

        let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
        put.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(42.5))));
        let mut alarm = PvStructure::new("alarm_t");
        alarm
            .fields
            .push(("severity".into(), PvField::Scalar(ScalarValue::Int(2)))); // MAJOR
        alarm
            .fields
            .push(("status".into(), PvField::Scalar(ScalarValue::Int(3))));
        put.fields.push(("alarm".into(), PvField::Structure(alarm)));
        let mut ts = PvStructure::new("time_t");
        ts.fields.push((
            "secondsPastEpoch".into(),
            PvField::Scalar(ScalarValue::Long(1_600_000_000)),
        ));
        ts.fields.push((
            "nanoseconds".into(),
            PvField::Scalar(ScalarValue::Int(123_456_789)),
        ));
        ts.fields
            .push(("userTag".into(), PvField::Scalar(ScalarValue::Int(7))));
        put.fields
            .push(("timeStamp".into(), PvField::Structure(ts)));

        source
            .put_value_ctx(
                "MB:PV",
                PvField::Structure(put),
                make_ctx("h", "anyone", "anonymous"),
            )
            .await
            .expect("full NTScalar PUT must succeed");

        // Read the snapshot back; the simple PV must have persisted the
        // client-supplied alarm + timeStamp, not local defaults.
        let snap = snapshot_for(&db, "MB:PV").await.unwrap();
        assert_eq!(snap.value, EpicsValue::Double(42.5), "value");
        assert_eq!(snap.alarm.severity, 2, "alarm.severity persisted");
        assert_eq!(snap.alarm.status, 3, "alarm.status persisted");
        assert_eq!(snap.user_tag, 7, "timeStamp.userTag persisted");
        let dur = snap.timestamp.since_unix_epoch();
        assert_eq!(dur.as_secs(), 1_600_000_000, "secondsPastEpoch persisted");
        assert_eq!(dur.subsec_nanos(), 123_456_789, "nanoseconds persisted");
    }

    /// The per-record ASL must gate `RULE(N, …)`
    /// rules. With `RULE(0, READ) RULE(1, WRITE)`, an ASL=0 record
    /// must be read-only (the WRITE rule does NOT apply when
    /// `record_asl < 1`), and an ASL=1 record must be writable.
    /// Pre-fix every record was treated as ASL=0 by the PVA path,
    /// so even an ASL=1 record fell under WRITE… but more subtly,
    /// an ASL=0 record ALSO got WRITE because `record_asl=0 ≤
    /// rule.level=1`. The fix doesn't change the equality bound;
    /// it threads the real ASL so `RULE(N, …)` with `record_asl > N`
    /// now skips the rule.
    #[epics_macros_rs::epics_test]
    async fn asl_gate_skips_rules_above_record_asl() {
        use epics_base_rs::server::records::ai::AiRecord;

        // RULE(2, WRITE) — only applies when record_asl ≤ 2.
        // RULE(0, READ) — always applies.
        let acf = parse_acf(
            r#"
ASG(DEFAULT) {
    RULE(0, READ)
    RULE(2, WRITE)
}
"#,
        )
        .unwrap();

        let db = Arc::new(PvDatabase::new());
        // High-ASL record: should be locked out of WRITE because
        // record_asl(3) > rule.level(2). Pre-fix the call site
        // always passed 0 and the rule was always applicable.
        db.add_record("AI:LOCKED", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        // ASL is a u8; the parser clamps to 0/1 via put_common_field,
        // but the underlying field accepts any u8 — set it directly
        // for the test to exercise the gate above C's 0/1 range.
        db.get_record("AI:LOCKED").unwrap().write().common.asl = 3;

        db.add_record("AI:OPEN", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        // OPEN keeps the default ASL=0; the WRITE rule applies.

        let source = PvDatabaseSource::new_with_acf(
            db.clone(),
            epics_base_rs::server::access_security::new_acf_cell(Some(acf)),
        );

        // Locked: WRITE rule skipped (record_asl 3 > rule.level 2),
        // so only READ applies — PUT must be denied.
        assert!(
            source
                .put_value_ctx(
                    "AI:LOCKED",
                    pv_double(1.0),
                    make_ctx("h", "anyone", "anonymous")
                )
                .await
                .is_err(),
            "ASL=3 record must NOT match RULE(2, WRITE)",
        );

        // Open: WRITE rule applies (record_asl 0 ≤ rule.level 2),
        // so PUT succeeds.
        source
            .put_value_ctx(
                "AI:OPEN",
                pv_double(2.0),
                make_ctx("h", "anyone", "anonymous"),
            )
            .await
            .expect("ASL=0 record must match RULE(2, WRITE)");
    }

    /// PVA-61 regression: a record-backed PROCESS must run the record's
    /// processing chain, not the no-op default success. A `CALC="5"`
    /// record evaluates the expression into VAL on process, so VAL flips
    /// from its unprocessed default (0) to 5 — proof the chain ran, which
    /// the old no-op `Ok(())` could never produce.
    #[epics_macros_rs::epics_test]
    async fn process_runs_record_processing_not_noop() {
        use epics_base_rs::server::records::calc::CalcRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("CALC:PROC", Box::new(CalcRecord::new("5")))
            .await
            .unwrap();
        let source = PvDatabaseSource::new(db.clone());
        let ctx = make_ctx("localhost", "op", "ca");

        // Unprocessed default.
        let before = source
            .get_value_ctx("CALC:PROC", ctx.clone())
            .await
            .expect("initial get");
        assert_eq!(
            extract_put_value_leaf(&before),
            Some(PvField::Scalar(ScalarValue::Double(0.0))),
            "VAL starts at the unprocessed default"
        );

        // PROCESS via the WRITE-gated checked path (no ACF → allowed).
        let checked = source
            .access()
            .check(
                "CALC:PROC",
                &ctx.creds.host,
                &ctx.creds.account,
                &ctx.creds.method,
                "",
            )
            .await;
        source
            .process_checked(checked, ctx.clone())
            .await
            .expect("PROCESS must succeed");

        // The CALC expression was evaluated into VAL: the chain ran.
        let after = source
            .get_value_ctx("CALC:PROC", ctx.clone())
            .await
            .expect("post-process get");
        assert_eq!(
            extract_put_value_leaf(&after),
            Some(PvField::Scalar(ScalarValue::Double(5.0))),
            "record PROCESS must evaluate CALC into VAL, not no-op"
        );
    }

    /// An external PVA PUT on a record-backed channel owes C `dbPutField`,
    /// not `dbPut`: a PASSIVE record must process on the put. Routing it
    /// through `put_pv` landed the value but never processed — UDF stayed
    /// set and no monitor event was posted, which is the whole
    /// `--phase pva-monitor` "0 update(s) after the seed" pattern.
    ///
    /// UDF is the witness that discriminates the two routes: `dbPut` writes
    /// the field and stops, so only a real process cycle clears it.
    #[epics_macros_rs::epics_test]
    async fn external_put_on_a_passive_record_processes_it() {
        use epics_base_rs::server::records::calc::CalcRecord;

        let db = Arc::new(PvDatabase::new());
        // CALC="A" with VAL driven by the put: processing recomputes VAL and
        // clears UDF. SCAN defaults to Passive.
        db.add_record("CALC:PUT", Box::new(CalcRecord::new("A")))
            .await
            .unwrap();
        let source = PvDatabaseSource::new(db.clone());
        let ctx = make_ctx("localhost", "op", "ca");

        assert_eq!(
            db.get_pv("CALC:PUT.UDF").expect("UDF get"),
            EpicsValue::UChar(1),
            "a record that has never processed is UDF"
        );

        let checked = source
            .access()
            .check(
                "CALC:PUT.A",
                &ctx.creds.host,
                &ctx.creds.account,
                &ctx.creds.method,
                "",
            )
            .await;
        let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
        put.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(3.0))));
        source
            .put_value_checked(checked, PvField::Structure(put), ctx.clone())
            .await
            .expect("PUT must succeed");

        assert_eq!(
            db.get_pv("CALC:PUT.UDF").expect("UDF get"),
            EpicsValue::UChar(0),
            "an external PUT owes dbPutField, which processes a PASSIVE record \
             and clears UDF — `put_pv` (dbPut) would leave it set"
        );
        assert_eq!(
            db.get_pv("CALC:PUT.VAL").expect("VAL get"),
            EpicsValue::Double(3.0),
            "processing must evaluate CALC=\"A\" into VAL"
        );
    }

    /// A record-process monitor update marks value/alarm/timeStamp and NOT
    /// the metadata leaves: pvxs assigns those only from the separate
    /// `DBE_PROPERTY` subscription (`singlesource.cpp:162-166`), so putting
    /// display/control/valueAlarm on every value update is a strict superset
    /// of C — the whole `--phase pva-monitor` `update_events` pattern.
    #[epics_macros_rs::epics_test]
    async fn a_process_update_marks_value_alarm_time_not_the_metadata_leaves() {
        use epics_base_rs::server::records::ai::AiRecord;

        let db = Arc::new(PvDatabase::new());
        // `ai` supplies every rset property slot, so a full-mask update would
        // mark display/control/valueAlarm — this record makes the superset
        // visible if it comes back.
        db.add_record("AI:MON", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let source = PvDatabaseSource::new(db.clone());
        let ctx = make_ctx("localhost", "op", "ca");
        let checked = source
            .access()
            .check(
                "AI:MON",
                &ctx.creds.host,
                &ctx.creds.account,
                &ctx.creds.method,
                "",
            )
            .await;

        let mut rx = source
            .subscribe_checked_opts_marked(checked, ctx, Default::default())
            .await
            .expect("record subscribe");

        // Drive one process cycle through the external-put owner.
        db.put_record_field_from_ca_no_notify("AI:MON", "VAL", EpicsValue::Double(1.0))
            .await
            .expect("put");

        let update =
            epics_base_rs::runtime::task::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("an update must post within 2s")
                .expect("stream open");
        let marked = update.marked.expect("a record update declares its marks");

        assert!(
            marked.iter().any(|m| m == "value"),
            "a DBE_VALUE post marks value, got {marked:?}"
        );
        assert!(
            marked.iter().any(|m| m == "timeStamp"),
            "getTimeAlarm always assigns timeStamp, got {marked:?}"
        );
        for leaf in marked.iter() {
            assert!(
                !leaf.starts_with("display.")
                    && !leaf.starts_with("control.")
                    && !leaf.starts_with("valueAlarm."),
                "a process update must mark no metadata leaf — those are the \
                 DBE_PROPERTY subscription's — but it marked {leaf}"
            );
        }
    }

    /// The event-class rule keys on the event's OWN mask, so the alarm triple
    /// rides `DBE_ALARM` only. Measured on `softIocPVX`: a driven `longin`
    /// posts value+alarm+timeStamp on update 1 (UDF -> NO_ALARM changed the
    /// alarm) and value+timeStamp on updates 2 and 3.
    #[test]
    fn the_alarm_leaf_rides_dbe_alarm_not_every_update() {
        use epics_base_rs::server::recgbl::EventMask;
        use epics_base_rs::server::snapshot::PropertySupport;

        let props = PropertySupport::NUMERIC;

        // A value-only post (what a record's monitor() sends when the alarm
        // did not change): DBE_VALUE|DBE_LOG.
        let value_only = crate::nt::event_leaves(EventMask::VALUE | EventMask::LOG, props, false);
        assert_eq!(
            value_only,
            vec!["timeStamp".to_string(), "value".to_string()],
            "a value post marks timeStamp+value — no alarm, no metadata"
        );

        // The same post with the alarm changed.
        let with_alarm = crate::nt::event_leaves(
            EventMask::VALUE | EventMask::LOG | EventMask::ALARM,
            props,
            false,
        );
        assert_eq!(
            with_alarm,
            vec![
                "timeStamp".to_string(),
                "alarm".to_string(),
                "value".to_string()
            ],
            "DBE_ALARM adds the alarm leaf and nothing else"
        );

        // A property post marks the metadata and NOT value/timeStamp.
        let property = crate::nt::event_leaves(EventMask::PROPERTY, props, false);
        assert!(
            property.iter().any(|l| l == "display.limitLow")
                && property.iter().any(|l| l == "control.limitLow"),
            "a DBE_PROPERTY post marks the getProperties leaves, got {property:?}"
        );
        assert!(
            !property.iter().any(|l| l == "value" || l == "timeStamp"),
            "a property-only post assigns no value/timeStamp, got {property:?}"
        );
    }

    // ── UpstreamMonitor boundaries ─────────────────────────────────────
    //
    // The three monitor bridge tasks became `MonitorStream::Upstream`
    // adapters that own the subscription and apply the transform on pull.
    // Three boundaries the deleted tasks used to hold, one test each:
    // the seed ordering, the empty-mask filter, and Empty-vs-Disconnected
    // on the non-blocking path (the whole reason the adapters exist).

    /// The `PvSubscription` bridge sent the connect-time snapshot before
    /// entering its loop. The adapter must hand the same value out first —
    /// and, unlike the task, without the consumer awaiting anything: this
    /// is the property that lets an RTEMS operation thread drain a monitor
    /// without a reactor.
    #[epics_macros_rs::epics_test]
    async fn the_upstream_seed_is_the_first_item_and_needs_no_await() {
        use tokio::sync::mpsc::error::TryRecvError;

        let db = Arc::new(PvDatabase::new());
        db.add_pv("SEED:MON", EpicsValue::Double(7.0))
            .await
            .unwrap();
        let src = PvDatabaseSource::new(db.clone());

        let mut rx = src
            .subscribe_ctx("SEED:MON", make_ctx("127.0.0.1", "anon", "ca"))
            .await
            .expect("subscribe to simple PV");

        let first = rx.try_recv().expect("the seed is available with no await");
        assert_eq!(
            monitor_double(&first),
            Some(7.0),
            "the first item is the connect-time snapshot"
        );
        assert!(
            matches!(rx.try_recv(), Err(TryRecvError::Empty)),
            "after the seed and before any post the stream is Empty, not \
             Disconnected — the producer is alive"
        );
    }

    /// The marked bridge's `if marked.is_empty() { continue; }`. The adapter
    /// expresses it as `map -> None`, so the filter has to survive as a
    /// property of the transform itself.
    #[test]
    fn an_event_whose_class_marks_nothing_is_filtered_out() {
        use epics_base_rs::server::recgbl::EventMask;
        use epics_base_rs::server::snapshot::Snapshot;

        let mk = |mask: EventMask| epics_base_rs::server::pv::MonitorEvent {
            snapshot: std::sync::Arc::new(Snapshot::new(
                EpicsValue::Double(1.0),
                0,
                0,
                std::time::SystemTime::now(),
            )),
            origin: 0,
            mask,
        };

        assert!(
            marked_update(mk(EventMask::from_bits(0))).is_none(),
            "a class that marks no leaf has no wire meaning — C assigns none \
             either, so it must not reach the client as an update"
        );
        let value = marked_update(mk(EventMask::VALUE)).expect("a DBE_VALUE post is an update");
        assert!(
            value
                .marked
                .expect("a record update declares its marks")
                .iter()
                .any(|m| m == "value"),
            "the positive control: a marking event still comes through"
        );
    }

    /// A record monitor with nothing posted yet reports `Empty` (park), and
    /// only reports `Disconnected` when the producer is really gone. Getting
    /// this backwards would make an RTEMS drain loop tear down every idle
    /// monitor, which is why the mapping is spelled once in `from_queue_err`.
    #[epics_macros_rs::epics_test]
    async fn an_idle_record_monitor_is_empty_not_disconnected() {
        use epics_base_rs::server::records::ai::AiRecord;
        use tokio::sync::mpsc::error::TryRecvError;

        let db = Arc::new(PvDatabase::new());
        db.add_record("AI:IDLE", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let source = PvDatabaseSource::new(db.clone());
        let ctx = make_ctx("localhost", "op", "ca");
        let checked = source
            .access()
            .check(
                "AI:IDLE",
                &ctx.creds.host,
                &ctx.creds.account,
                &ctx.creds.method,
                "",
            )
            .await;

        let mut rx = source
            .subscribe_checked_opts_marked(checked, ctx, Default::default())
            .await
            .expect("record subscribe");

        assert!(
            matches!(rx.try_recv(), Err(TryRecvError::Empty)),
            "an armed record monitor with no post yet is Empty"
        );

        db.put_record_field_from_ca_no_notify("AI:IDLE", "VAL", EpicsValue::Double(1.0))
            .await
            .expect("put");
        epics_base_rs::runtime::task::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("an update must post within 2s")
            .expect("stream open");
    }

    /// A simple/mailbox PV has no record body; PROCESS reports unsupported
    /// rather than silently succeeding.
    #[epics_macros_rs::epics_test]
    async fn process_simple_pv_is_unsupported() {
        let db = Arc::new(PvDatabase::new());
        db.add_pv("MAILBOX:PV", EpicsValue::Double(1.0))
            .await
            .unwrap();
        let source = PvDatabaseSource::new(db.clone());
        let ctx = make_ctx("localhost", "op", "ca");
        let checked = source
            .access()
            .check(
                "MAILBOX:PV",
                &ctx.creds.host,
                &ctx.creds.account,
                &ctx.creds.method,
                "",
            )
            .await;
        assert!(
            source.process_checked(checked, ctx).await.is_err(),
            "PROCESS on a simple PV must be unsupported, not silent success"
        );
    }
}
