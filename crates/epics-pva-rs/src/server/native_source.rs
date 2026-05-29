//! [`ChannelSource`] implementation backed by an epics-rs [`PvDatabase`].
//!
//! Replaces the spvirit-shaped `bridge.rs::PvDatabaseStore`. Builds NTScalar
//! and NTScalarArray `PvField` values directly from `Snapshot`s, with full
//! alarm/timeStamp/display metadata.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::client_native::context::PvGetResult; // not used; kept for re-export hygiene
use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use crate::server_native::{ChannelSource, OpError};

use epics_base_rs::server::access_security::AccessSecurityConfig;
use epics_base_rs::server::database::{PvDatabase, PvEntry, parse_pv_name};
use epics_base_rs::server::recgbl::{alarm_condition_string, alarm_status};
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::EpicsValue;
use tokio::sync::RwLock;

/// Shared, mutable ACF cell. Changed from
/// `Arc<Option<AccessSecurityConfig>>` to `Arc<RwLock<...>>` so
/// `PvaServer::reload_acf_from` can swap the policy at runtime
/// (mirrors `CaServer::reload_acf`). All `PvDatabaseSource` ACF
/// check sites acquire a read guard; the `/reload-acf` introspection
/// endpoint and any future site-policy SIGHUP handler acquire a
/// write guard via `PvaServer::reload_acf_from`.
pub type AcfCell = Arc<RwLock<Option<AccessSecurityConfig>>>;

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
        let acf: AcfCell = Arc::new(RwLock::new(None));
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
                if let Some(rec) = db.get_record(base).await {
                    let inst = rec.read().await;
                    let asg = if !inst.common.asg.is_empty() {
                        inst.common.asg.clone()
                    } else {
                        "DEFAULT".to_string()
                    };
                    return (asg, inst.common.asl);
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
                let rec = db.get_record(base).await?;
                let inst = rec.read().await;
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
    let value_field = match &snap.value {
        EpicsValue::Double(v) => PvField::Scalar(ScalarValue::Double(*v)),
        EpicsValue::Float(v) => PvField::Scalar(ScalarValue::Float(*v)),
        EpicsValue::Long(v) => PvField::Scalar(ScalarValue::Int(*v)),
        EpicsValue::Short(v) => PvField::Scalar(ScalarValue::Short(*v)),
        EpicsValue::Char(v) => PvField::Scalar(ScalarValue::UByte(*v)),
        EpicsValue::Enum(v) => PvField::Scalar(ScalarValue::Int(*v as i32)),
        EpicsValue::String(s) => PvField::Scalar(ScalarValue::String(s.clone())),
        EpicsValue::Int64(v) => PvField::Scalar(ScalarValue::Long(*v)),
        // C `DBF_UINT64` → PVA `ulong` (native unsigned 64-bit).
        EpicsValue::UInt64(v) => PvField::Scalar(ScalarValue::ULong(*v)),
        EpicsValue::DoubleArray(v) => {
            PvField::ScalarArray(v.iter().map(|x| ScalarValue::Double(*x)).collect())
        }
        EpicsValue::FloatArray(v) => {
            PvField::ScalarArray(v.iter().map(|x| ScalarValue::Float(*x)).collect())
        }
        EpicsValue::LongArray(v) => {
            PvField::ScalarArray(v.iter().map(|x| ScalarValue::Int(*x)).collect())
        }
        EpicsValue::ShortArray(v) => {
            PvField::ScalarArray(v.iter().map(|x| ScalarValue::Short(*x)).collect())
        }
        EpicsValue::CharArray(v) => {
            PvField::ScalarArray(v.iter().map(|x| ScalarValue::UByte(*x)).collect())
        }
        EpicsValue::EnumArray(v) => {
            PvField::ScalarArray(v.iter().map(|x| ScalarValue::Int(*x as i32)).collect())
        }
        EpicsValue::StringArray(v) => {
            PvField::ScalarArray(v.iter().map(|x| ScalarValue::String(x.clone())).collect())
        }
        EpicsValue::Int64Array(v) => {
            PvField::ScalarArray(v.iter().map(|x| ScalarValue::Long(*x)).collect())
        }
        EpicsValue::UInt64Array(v) => {
            PvField::ScalarArray(v.iter().map(|x| ScalarValue::ULong(*x)).collect())
        }
    };

    let is_array = matches!(value_field, PvField::ScalarArray(_));
    let struct_id = if is_array {
        "epics:nt/NTScalarArray:1.0"
    } else {
        "epics:nt/NTScalar:1.0"
    };

    let mut s = PvStructure::new(struct_id);
    s.fields.push(("value".into(), value_field));
    s.fields.push(("alarm".into(), build_alarm(snap)));
    s.fields.push(("timeStamp".into(), build_timestamp(snap)));
    s.fields.push(("display".into(), build_display(snap)));
    if !is_array {
        s.fields.push(("control".into(), build_control(snap)));
        s.fields
            .push(("valueAlarm".into(), build_value_alarm(snap)));
    }
    PvField::Structure(s)
}

fn snapshot_to_field_desc(snap: &Snapshot) -> FieldDesc {
    // Mirror `snapshot_to_pv_field`: a DBR_ENUM scalar advertises the
    // NTEnum descriptor (`epics:nt/NTEnum:1.0`) so value and introspection
    // stay in lockstep (pvxs `ioc/singlesource.cpp:200-201`, `src/nt.cpp:121-131`).
    if matches!(&snap.value, EpicsValue::Enum(_)) {
        return nt_enum_desc();
    }
    let (value_desc, is_array) = match &snap.value {
        EpicsValue::Double(_) => (FieldDesc::Scalar(ScalarType::Double), false),
        EpicsValue::Float(_) => (FieldDesc::Scalar(ScalarType::Float), false),
        EpicsValue::Long(_) => (FieldDesc::Scalar(ScalarType::Int), false),
        EpicsValue::Short(_) => (FieldDesc::Scalar(ScalarType::Short), false),
        EpicsValue::Char(_) => (FieldDesc::Scalar(ScalarType::UByte), false),
        EpicsValue::Enum(_) => (FieldDesc::Scalar(ScalarType::Int), false),
        EpicsValue::String(_) => (FieldDesc::Scalar(ScalarType::String), false),
        EpicsValue::Int64(_) => (FieldDesc::Scalar(ScalarType::Long), false),
        EpicsValue::UInt64(_) => (FieldDesc::Scalar(ScalarType::ULong), false),
        EpicsValue::DoubleArray(_) => (FieldDesc::ScalarArray(ScalarType::Double), true),
        EpicsValue::FloatArray(_) => (FieldDesc::ScalarArray(ScalarType::Float), true),
        EpicsValue::LongArray(_) => (FieldDesc::ScalarArray(ScalarType::Int), true),
        EpicsValue::ShortArray(_) => (FieldDesc::ScalarArray(ScalarType::Short), true),
        EpicsValue::CharArray(_) => (FieldDesc::ScalarArray(ScalarType::UByte), true),
        EpicsValue::EnumArray(_) => (FieldDesc::ScalarArray(ScalarType::Int), true),
        EpicsValue::StringArray(_) => (FieldDesc::ScalarArray(ScalarType::String), true),
        EpicsValue::Int64Array(_) => (FieldDesc::ScalarArray(ScalarType::Long), true),
        EpicsValue::UInt64Array(_) => (FieldDesc::ScalarArray(ScalarType::ULong), true),
    };
    let struct_id = if is_array {
        "epics:nt/NTScalarArray:1.0"
    } else {
        "epics:nt/NTScalar:1.0"
    };
    let mut fields = vec![
        ("value".to_string(), value_desc),
        ("alarm".into(), alarm_desc()),
        ("timeStamp".into(), timestamp_desc()),
        ("display".into(), display_desc()),
    ];
    if !is_array {
        fields.push(("control".into(), control_desc()));
        fields.push(("valueAlarm".into(), value_alarm_desc()));
    }
    FieldDesc::Structure {
        struct_id: struct_id.into(),
        fields,
    }
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
    // (no `display_t` id, no limits/units) — distinct from the NTScalar display.
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

fn display_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "display_t".into(),
        fields: vec![
            ("limitLow".into(), FieldDesc::Scalar(ScalarType::Double)),
            ("limitHigh".into(), FieldDesc::Scalar(ScalarType::Double)),
            ("description".into(), FieldDesc::Scalar(ScalarType::String)),
            ("units".into(), FieldDesc::Scalar(ScalarType::String)),
            ("precision".into(), FieldDesc::Scalar(ScalarType::Int)),
        ],
    }
}

fn control_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "control_t".into(),
        fields: vec![
            ("limitLow".into(), FieldDesc::Scalar(ScalarType::Double)),
            ("limitHigh".into(), FieldDesc::Scalar(ScalarType::Double)),
            ("minStep".into(), FieldDesc::Scalar(ScalarType::Double)),
        ],
    }
}

fn value_alarm_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "valueAlarm_t".into(),
        fields: vec![
            ("active".into(), FieldDesc::Scalar(ScalarType::Boolean)),
            (
                "lowAlarmLimit".into(),
                FieldDesc::Scalar(ScalarType::Double),
            ),
            (
                "lowWarningLimit".into(),
                FieldDesc::Scalar(ScalarType::Double),
            ),
            (
                "highWarningLimit".into(),
                FieldDesc::Scalar(ScalarType::Double),
            ),
            (
                "highAlarmLimit".into(),
                FieldDesc::Scalar(ScalarType::Double),
            ),
            (
                "lowAlarmSeverity".into(),
                FieldDesc::Scalar(ScalarType::Int),
            ),
            (
                "lowWarningSeverity".into(),
                FieldDesc::Scalar(ScalarType::Int),
            ),
            (
                "highWarningSeverity".into(),
                FieldDesc::Scalar(ScalarType::Int),
            ),
            (
                "highAlarmSeverity".into(),
                FieldDesc::Scalar(ScalarType::Int),
            ),
            ("hysteresis".into(), FieldDesc::Scalar(ScalarType::UByte)),
        ],
    }
}

fn build_alarm(snap: &Snapshot) -> PvField {
    let mut a = PvStructure::new("alarm_t");
    a.fields.push((
        "severity".into(),
        PvField::Scalar(ScalarValue::Int(snap.alarm.severity as i32)),
    ));
    a.fields.push((
        "status".into(),
        PvField::Scalar(ScalarValue::Int(snap.alarm.status as i32)),
    ));
    // pvxs `iocsource.cpp:226,236`: `alarm.message` is the alarm
    // condition string for a non-zero status, "" when NO_ALARM. The
    // amsg-preference path (DBR_AMSG → `meta.amsg`) is not reproduced
    // here because `AlarmInfo`/`Snapshot` does not carry the record's
    // explicit amsg — plumbing `CommonFields.amsg` through the snapshot
    // builders is a separate base-rs change (see UNFIXED note).
    let message = if snap.alarm.status == alarm_status::NO_ALARM {
        String::new()
    } else {
        alarm_condition_string(snap.alarm.status).to_string()
    };
    a.fields.push((
        "message".into(),
        PvField::Scalar(ScalarValue::String(message)),
    ));
    PvField::Structure(a)
}

fn build_timestamp(snap: &Snapshot) -> PvField {
    // pvxs `iocsource.cpp:240-248`: `timeStamp` carries the record's
    // acquisition time and userTag, NOT the serialization wall-clock.
    // `Snapshot.timestamp` is the acquisition `SystemTime` (POSIX epoch;
    // the codec already added POSIX_TIME_AT_EPICS_EPOCH on decode) and
    // `Snapshot.user_tag` is the nsec-LSB / pulse-id tag that
    // `apply_nsec_lsb_split` strips out of `nanoseconds` (mirroring
    // pvxs `meta.time.nsec & ~info.nsecMask` for the wire nanoseconds and
    // `meta.time.nsec & info.nsecMask` for userTag). Using `now()` here
    // overwrote the acquisition time with serialization time and zeroed
    // the userTag on every record-backed GET/MONITOR.
    let dur = snap
        .timestamp
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
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

fn build_display(snap: &Snapshot) -> PvField {
    let mut d = PvStructure::new("display_t");
    // pvxs `iocsource.cpp:306-308` sets `display.description` from the
    // record's DESC field; in Rust it is `DisplayInfo.description`,
    // one field over from the limits/units this already reads. It was
    // hardcoded empty even when the snapshot carried a description.
    let (lo, hi, desc, units, prec) = if let Some(disp) = &snap.display {
        (
            disp.lower_disp_limit,
            disp.upper_disp_limit,
            disp.description.clone(),
            disp.units.clone(),
            disp.precision as i32,
        )
    } else {
        (0.0, 0.0, String::new(), String::new(), 0)
    };
    d.fields
        .push(("limitLow".into(), PvField::Scalar(ScalarValue::Double(lo))));
    d.fields
        .push(("limitHigh".into(), PvField::Scalar(ScalarValue::Double(hi))));
    d.fields.push((
        "description".into(),
        PvField::Scalar(ScalarValue::String(desc)),
    ));
    d.fields
        .push(("units".into(), PvField::Scalar(ScalarValue::String(units))));
    d.fields
        .push(("precision".into(), PvField::Scalar(ScalarValue::Int(prec))));
    PvField::Structure(d)
}

fn build_control(snap: &Snapshot) -> PvField {
    let mut c = PvStructure::new("control_t");
    let (lo, hi) = if let Some(ctrl) = &snap.control {
        (ctrl.lower_ctrl_limit, ctrl.upper_ctrl_limit)
    } else {
        (0.0, 0.0)
    };
    c.fields
        .push(("limitLow".into(), PvField::Scalar(ScalarValue::Double(lo))));
    c.fields
        .push(("limitHigh".into(), PvField::Scalar(ScalarValue::Double(hi))));
    c.fields
        .push(("minStep".into(), PvField::Scalar(ScalarValue::Double(0.0))));
    PvField::Structure(c)
}

fn build_value_alarm(snap: &Snapshot) -> PvField {
    // pvxs `iocsource.cpp:300-303` fills the four valueAlarm limits from
    // DBR_AL_DOUBLE; in Rust they live in
    // `DisplayInfo.{lower,upper}_{alarm,warning}_limit`. These were
    // hardcoded 0.0 here even when the snapshot carried them. The
    // per-limit severities / `active` / `hysteresis` are not part of
    // DBR_AL_DOUBLE (pvxs leaves them untouched in this path), so they
    // remain 0/false until a record exposes LSV/HSV etc.
    let mut v = PvStructure::new("valueAlarm_t");
    v.fields.push((
        "active".into(),
        PvField::Scalar(ScalarValue::Boolean(false)),
    ));
    let limits: [(&str, f64); 4] = match &snap.display {
        Some(d) => [
            ("lowAlarmLimit", d.lower_alarm_limit),
            ("lowWarningLimit", d.lower_warning_limit),
            ("highWarningLimit", d.upper_warning_limit),
            ("highAlarmLimit", d.upper_alarm_limit),
        ],
        None => [
            ("lowAlarmLimit", 0.0),
            ("lowWarningLimit", 0.0),
            ("highWarningLimit", 0.0),
            ("highAlarmLimit", 0.0),
        ],
    };
    for (name, val) in limits {
        v.fields
            .push((name.into(), PvField::Scalar(ScalarValue::Double(val))));
    }
    for name in [
        "lowAlarmSeverity",
        "lowWarningSeverity",
        "highWarningSeverity",
        "highAlarmSeverity",
    ] {
        v.fields
            .push((name.into(), PvField::Scalar(ScalarValue::Int(0))));
    }
    v.fields
        .push(("hysteresis".into(), PvField::Scalar(ScalarValue::UByte(0))));
    PvField::Structure(v)
}

// ── ChannelSource impl ────────────────────────────────────────────────────

async fn snapshot_for(db: &PvDatabase, name: &str) -> Option<Snapshot> {
    let (_base, field) = parse_pv_name(name);
    match db.find_entry(name).await? {
        PvEntry::Simple(pv) => Some(pv.snapshot().await),
        PvEntry::Record(rec) => {
            let inst = rec.read().await;
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
            names.extend(db.all_alias_names().await);
            names
        }
    }

    fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
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

    fn put_value(
        &self,
        name: &str,
        value: PvField,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let db = self.db.clone();
        let name = name.to_string();
        async move {
            // Extract the inner value field (NTScalar.value or top-level scalar).
            // The WRITE gate ran in `put_value_checked` before reaching here,
            // so every failure below is operational (Failed), not a denial.
            let scalar = match &value {
                PvField::Structure(s) => match s.get_field("value") {
                    // NTEnum PUT: `value` is an `enum_t` struct; pvxs
                    // dereferences the put through `value.index` before
                    // converting to the DBR enum (`ioc/iocsource.cpp:589-593`).
                    // The bare-scalar NTScalar PUT keeps `value` directly.
                    Some(PvField::Structure(inner)) => inner.get_field("index").cloned(),
                    other => other.cloned(),
                },
                _ => Some(value),
            };
            let scalar = scalar.ok_or_else(|| OpError::failed("PUT missing 'value' field"))?;
            let epics = pv_field_to_epics(&scalar)
                .ok_or_else(|| OpError::failed("PUT value not representable as EpicsValue"))?;
            db.put_pv(&name, epics)
                .await
                .map_err(|e| OpError::failed(e.to_string()))
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

    fn subscribe(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send {
        let db = self.db.clone();
        let name = name.to_string();
        async move {
            let (tx, rx) = mpsc::channel::<PvField>(64);
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
                        Some(mut sub) => {
                            let initial = snapshot_to_pv_field(&pv.snapshot().await);
                            tokio::spawn(async move {
                                if tx.send(initial).await.is_err() {
                                    return;
                                }
                                while let Some(snap) = sub.recv_snapshot().await {
                                    let field = snapshot_to_pv_field(&snap);
                                    if tx.send(field).await.is_err() {
                                        break;
                                    }
                                }
                                // `sub` drops here → its `Drop` removes the
                                // subscriber slot from the ProcessVariable.
                            });
                        }
                        None => {
                            // Per-PV subscriber cap reached: still honour the
                            // connect-time read so the client at least sees
                            // the current value.
                            let initial = snapshot_to_pv_field(&pv.snapshot().await);
                            let _ = tx.send(initial).await;
                        }
                    }
                }
                PvEntry::Record(_rec) => {
                    // Subscribe via the public DbSubscription API.
                    use epics_base_rs::server::database::db_access::DbSubscription;
                    let mut sub = match DbSubscription::subscribe(&db, &name).await {
                        Some(s) => s,
                        None => return None,
                    };
                    tokio::spawn(async move {
                        while let Some(snap) = sub.recv_snapshot().await {
                            let pv = snapshot_to_pv_field(&snap);
                            if tx.send(pv).await.is_err() {
                                break;
                            }
                        }
                    });
                }
            }
            Some(rx)
        }
    }
}

// ── PvField → EpicsValue (PUT path) ────────────────────────────────────

fn pv_field_to_epics(field: &PvField) -> Option<EpicsValue> {
    match field {
        PvField::Scalar(sv) => Some(scalar_to_epics(sv)),
        PvField::ScalarArray(items) => scalar_array_to_epics(items),
        // the PVA wire decoder delivers a decoded scalar array
        // as the refcount-shared `ScalarArrayTyped` form, not the
        // generic `ScalarArray`. The earlier arm only matched
        // `ScalarArray`, so a real wire `ulong[]` (or any typed
        // array) PUT fell through to `None` and was rejected. Route
        // the typed form through the same converter.
        PvField::ScalarArrayTyped(t) => scalar_array_to_epics(&t.to_scalar_values()),
        _ => None,
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
#[allow(unused_imports)]
type _Pvr = PvGetResult;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_native::ChannelContext;
    use epics_base_rs::server::access_security::parse_acf;

    fn make_ctx(host: &str, account: &str, method: &str) -> ChannelContext {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        ChannelContext {
            peer: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0),
            account: account.to_string(),
            method: method.to_string(),
            host: host.to_string(),
            authority: String::new(),
            roles: Vec::new(),
            pv_request: None,
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
        ) -> Option<tokio::sync::mpsc::Receiver<PvField>>;
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
                .check(pv, &ctx.host, &ctx.account, &ctx.method, "")
                .await;
            self.put_value_checked(checked, value, ctx)
                .await
                .map_err(String::from)
        }
        async fn get_value_ctx(&self, pv: &str, ctx: ChannelContext) -> Option<PvField> {
            let checked = self
                .access()
                .check(pv, &ctx.host, &ctx.account, &ctx.method, "")
                .await;
            self.get_value_checked(checked, ctx).await
        }
        async fn subscribe_ctx(
            &self,
            pv: &str,
            ctx: ChannelContext,
        ) -> Option<tokio::sync::mpsc::Receiver<PvField>> {
            let checked = self
                .access()
                .check(pv, &ctx.host, &ctx.account, &ctx.method, "")
                .await;
            self.subscribe_checked(checked, ctx).await
        }
    }

    fn pv_double(v: f64) -> PvField {
        // Top-level scalar PvField — put_value / put_value_ctx accept
        // both NTScalar and bare scalar.
        PvField::Scalar(ScalarValue::Double(v))
    }

    /// Look up a scalar sub-field of a `time_t`/`display_t`/`valueAlarm_t`
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

    #[test]
    fn build_timestamp_uses_snapshot_acquisition_time_and_user_tag() {
        // pvxs `iocsource.cpp:240-248`: timeStamp carries the record's
        // acquisition time + userTag, not the serialization wall-clock.
        let ts = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_789);
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
        let PvField::Structure(v) = build_value_alarm(&snap) else {
            panic!("valueAlarm must be a structure");
        };
        let get = |name: &str| match scalar(&v, name) {
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
        let PvField::Structure(d) = build_display(&snap) else {
            panic!("display must be a structure");
        };
        assert!(matches!(
            scalar(&d, "description"),
            ScalarValue::String(s) if s == "chamber pressure"
        ));
        assert!(matches!(
            scalar(&d, "units"),
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
    fn enum_snapshot_builds_nt_enum_value_and_desc() {
        // pvxs `ioc/singlesource.cpp:200-201`: a DBR_ENUM scalar surfaces
        // as `epics:nt/NTEnum:1.0` with `value.index` + `value.choices`,
        // not a numeric NTScalar.
        use epics_base_rs::server::snapshot::EnumInfo;
        let mut snap = Snapshot::new(EpicsValue::Enum(1), 0, 0, std::time::UNIX_EPOCH);
        snap.enums = Some(EnumInfo {
            strings: vec!["OFF".into(), "ON".into()],
        });

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

    #[tokio::test]
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

    /// Regression: a monitor on a *simple* native PV must observe later
    /// PUTs, not just the connect-time snapshot. Pre-fix the `Simple` arm
    /// sent one snapshot and dropped the channel, so a PVA PUT through the
    /// same server never reached the monitor. pvxs `SharedPV::post()` fans
    /// every update out to its stored subscribers (`sharedpv.cpp:417-440`).
    #[tokio::test]
    async fn simple_pv_monitor_observes_later_puts() {
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

        let updated = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
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
    #[tokio::test]
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
        let rec = db.get_record("AI:SECURE").await.unwrap();
        rec.write().await.common.asg = "SECURE".to_string();

        let source = PvDatabaseSource::new_with_acf(
            db.clone(),
            Arc::new(tokio::sync::RwLock::new(Some(acf))),
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
    #[tokio::test]
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
        let rec = db.get_record("AI:LOCKED").await.unwrap();
        rec.write().await.common.asg = "LOCKED".to_string();

        let source = PvDatabaseSource::new_with_acf(
            db.clone(),
            Arc::new(tokio::sync::RwLock::new(Some(acf))),
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
    #[tokio::test]
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
        let rec = db.get_record("AI:MON").await.unwrap();
        rec.write().await.common.asg = "LOCKED".to_string();

        let source = PvDatabaseSource::new_with_acf(
            db.clone(),
            Arc::new(tokio::sync::RwLock::new(Some(acf))),
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
    #[tokio::test]
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
        let cell: AcfCell = Arc::new(RwLock::new(Some(lockdown)));

        let db = Arc::new(PvDatabase::new());
        db.add_record("AI:LIVE", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let rec = db.get_record("AI:LIVE").await.unwrap();
        rec.write().await.common.asg = "SECURE".to_string();

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
        *cell.write().await = Some(permissive);

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
    #[tokio::test]
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
    #[tokio::test]
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
        db.get_record("AI:LOCKED")
            .await
            .unwrap()
            .write()
            .await
            .common
            .asg = "LOCKED".into();

        let source = PvDatabaseSource::new_with_acf(
            db.clone(),
            Arc::new(tokio::sync::RwLock::new(Some(acf))),
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
    #[tokio::test]
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
    #[tokio::test]
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
    #[tokio::test]
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
    #[tokio::test]
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
    #[tokio::test]
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
    #[tokio::test]
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
    #[tokio::test]
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
        db.get_record("AI:LOCKED")
            .await
            .unwrap()
            .write()
            .await
            .common
            .asl = 3;

        db.add_record("AI:OPEN", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        // OPEN keeps the default ASL=0; the WRITE rule applies.

        let source = PvDatabaseSource::new_with_acf(
            db.clone(),
            Arc::new(tokio::sync::RwLock::new(Some(acf))),
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
}
