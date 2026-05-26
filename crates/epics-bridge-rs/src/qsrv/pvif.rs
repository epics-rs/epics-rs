//! PVIF: PVData Interface — converts between EPICS record state and PVA structures.
//!
//! Corresponds to C++ QSRV's `pvif.h/pvif.cpp` (ScalarBuilder, etc.).

use std::time::{SystemTime, UNIX_EPOCH};

use epics_base_rs::server::snapshot::{ControlInfo, DisplayInfo, Snapshot};
use epics_base_rs::types::EpicsValue;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};

use crate::convert::{epics_to_pv_field, epics_to_scalar};

/// Field mapping type, corresponding to C++ QSRV PVIFBuilder types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldMapping {
    /// NTScalar/NTScalarArray with full metadata (alarm, timestamp, display, control)
    Scalar,
    /// Value only, no metadata
    Plain,
    /// Alarm + timestamp only, no value
    Meta,
    /// Variant union wrapping
    Any,
    /// Process-only: put triggers record processing, no value transfer
    Proc,
    /// Nested structure placeholder — no channel, defines an intermediate
    /// node in the group schema (pvxs `MappingInfo::Structure`).
    Structure,
    /// Constant value — no channel, set once at subscription setup and
    /// never changes (pvxs `MappingInfo::Const`).
    Const,
}

/// NormativeType classification derived from record type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NtType {
    /// ai, ao, longin, longout, stringin, stringout, calc, calcout
    Scalar,
    /// bi, bo, mbbi, mbbo
    Enum,
    /// waveform, compress, histogram
    ScalarArray,
}

impl NtType {
    /// Determine NtType from EPICS record type name.
    pub fn from_record_type(rtyp: &str) -> Self {
        match rtyp {
            "bi" | "bo" | "mbbi" | "mbbo" => NtType::Enum,
            "waveform" | "compress" | "histogram" => NtType::ScalarArray,
            _ => NtType::Scalar,
        }
    }
}

// ---------------------------------------------------------------------------
// Scalar-type classification
// ---------------------------------------------------------------------------

/// `display.form.choices` — the fixed seven-entry menu pvxs publishes for
/// every numeric NTScalar / NTScalarArray (`Q:form` info-tag menu).
///
/// Mirrors pvxs `ioc/iocsource.cpp:43-51` (`IOCSource::initialize`).
const FORM_CHOICES: [&str; 7] = [
    "Default",
    "String",
    "Binary",
    "Decimal",
    "Hex",
    "Exponential",
    "Engineering",
];

/// Derive the PVA scalar type of an `EpicsValue`, taking the element type
/// for array variants. This is the Rust equivalent of pvxs
/// `TypeCode::scalarOf()` applied to the NTScalar `value` member: metadata
/// limit fields (`display`, `control`, `valueAlarm`) are typed with this
/// scalar type, not hard-coded `double`.
fn value_scalar_type(value: &EpicsValue) -> ScalarType {
    match value {
        EpicsValue::String(_) | EpicsValue::StringArray(_) => ScalarType::String,
        EpicsValue::Short(_) | EpicsValue::ShortArray(_) => ScalarType::Short,
        EpicsValue::Float(_) | EpicsValue::FloatArray(_) => ScalarType::Float,
        EpicsValue::Enum(_) | EpicsValue::EnumArray(_) => ScalarType::UShort,
        // DBF_CHAR ↔ pvByte (signed), matching `convert::dbf_to_scalar_type`.
        EpicsValue::Char(_) | EpicsValue::CharArray(_) => ScalarType::Byte,
        EpicsValue::Long(_) | EpicsValue::LongArray(_) => ScalarType::Int,
        EpicsValue::Double(_) | EpicsValue::DoubleArray(_) => ScalarType::Double,
        EpicsValue::Int64(_) | EpicsValue::Int64Array(_) => ScalarType::Long,
        // C `DBF_UINT64` → PVA `ulong`.
        EpicsValue::UInt64(_) | EpicsValue::UInt64Array(_) => ScalarType::ULong,
    }
}

/// True for PVA scalar types that pvxs treats as numeric
/// (`Kind::Integer || Kind::Real`). pvxs only emits `display` limit
/// fields, `control`, and `valueAlarm` for numeric NTScalar values; a
/// string `value` carries `display = {description, units}` only.
///
/// Mirrors pvxs `src/nt.cpp:55` (`const bool isnumeric = ...`).
fn is_numeric_scalar(t: ScalarType) -> bool {
    !matches!(t, ScalarType::String)
}

/// Build a metadata limit `ScalarValue` of the value's scalar type from a
/// stored `f64` limit. pvxs types `display`/`control`/`valueAlarm` limits
/// with `value.scalarOf()`, so an `int32_t[]` waveform gets `int32_t`
/// limits and a `uint64_t` field gets `uint64_t` limits.
///
/// Mirrors pvxs `src/nt.cpp:61-104` (`Member(scalar, "limit*")`).
fn limit_scalar(t: ScalarType, v: f64) -> ScalarValue {
    match t {
        ScalarType::Boolean => ScalarValue::Boolean(v != 0.0),
        ScalarType::Byte => ScalarValue::Byte(v as i8),
        ScalarType::Short => ScalarValue::Short(v as i16),
        ScalarType::Int => ScalarValue::Int(v as i32),
        ScalarType::Long => ScalarValue::Long(v as i64),
        ScalarType::UByte => ScalarValue::UByte(v as u8),
        ScalarType::UShort => ScalarValue::UShort(v as u16),
        ScalarType::UInt => ScalarValue::UInt(v as u32),
        ScalarType::ULong => ScalarValue::ULong(v as u64),
        ScalarType::Float => ScalarValue::Float(v as f32),
        ScalarType::Double => ScalarValue::Double(v),
        // String values are non-numeric and never reach the limit
        // builders; fall back to the textual rendering for completeness.
        ScalarType::String => ScalarValue::String(v.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Snapshot → PvStructure conversion
// ---------------------------------------------------------------------------

/// Convert a Snapshot into an NTScalar PvStructure.
///
/// Structure ID: `epics:nt/NTScalar:1.0`
/// Fields: value, alarm, timeStamp, display (optional), control (optional)
pub fn snapshot_to_nt_scalar(snapshot: &Snapshot) -> PvStructure {
    let mut pv = PvStructure::new("epics:nt/NTScalar:1.0");

    // F6: an empty array landing in NTScalar conversion can't yield a real
    // scalar — `epics_to_scalar` will fall back to 0/0.0/"". Surface that as
    // INVALID/UDF so clients don't treat the placeholder as a valid reading.
    let empty_array = is_empty_array(&snapshot.value);

    // BR-R12: metadata limit fields take the value's scalar type, and the
    // metadata field *set* depends on whether that type is numeric.
    let scalar_type = value_scalar_type(&snapshot.value);
    let numeric = is_numeric_scalar(scalar_type);

    // value
    pv.fields.push((
        "value".into(),
        PvField::Scalar(epics_to_scalar(&snapshot.value)),
    ));

    // alarm
    let alarm_struct = if empty_array {
        build_alarm_overlay(snapshot, /*severity*/ 3, /*status (UDF)*/ 17)
    } else {
        build_alarm(snapshot)
    };
    pv.fields
        .push(("alarm".into(), PvField::Structure(alarm_struct)));

    // timeStamp
    pv.fields.push((
        "timeStamp".into(),
        PvField::Structure(build_timestamp(snapshot.timestamp, snapshot.user_tag)),
    ));

    // display
    if let Some(ref disp) = snapshot.display {
        pv.fields.push((
            "display".into(),
            PvField::Structure(build_display(disp, scalar_type, numeric)),
        ));
    }

    // control — pvxs emits this only for numeric values (src/nt.cpp:87).
    if numeric {
        if let Some(ref ctrl) = snapshot.control {
            pv.fields.push((
                "control".into(),
                PvField::Structure(build_control(ctrl, scalar_type)),
            ));
        }
    }

    // valueAlarm — pvxs emits this only for numeric values (src/nt.cpp:97).
    if numeric {
        if let Some(ref disp) = snapshot.display {
            pv.fields.push((
                "valueAlarm".into(),
                PvField::Structure(build_value_alarm(disp, scalar_type)),
            ));
        }
    }

    pv
}

/// Convert a Snapshot into an NTEnum PvStructure.
///
/// Structure ID: `epics:nt/NTEnum:1.0`
/// Fields: value{index, choices}, alarm, timeStamp, display{description}
///
/// BR-R22: pvxs's QSRV NTEnum (testqsingle.cpp:174) uses
/// `value.index int32_t` (not ushort) and includes a trailing
/// `display.description` field. Aligning the runtime shape and
/// descriptor with that prevents pvxs clients from seeing
/// a wrong-type index and missing the description field.
pub fn snapshot_to_nt_enum(snapshot: &Snapshot) -> PvStructure {
    let mut pv = PvStructure::new("epics:nt/NTEnum:1.0");

    // value sub-structure with index + choices
    let index = match &snapshot.value {
        EpicsValue::Enum(v) => *v as i32,
        EpicsValue::Short(v) => *v as i32,
        other => other.to_f64().map(|f| f as i32).unwrap_or(0),
    };

    let choices: Vec<ScalarValue> = snapshot
        .enums
        .as_ref()
        .map(|e| {
            e.strings
                .iter()
                .map(|s| ScalarValue::String(s.clone()))
                .collect()
        })
        .unwrap_or_default();

    let mut value_struct = PvStructure::new("enum_t");
    value_struct
        .fields
        .push(("index".into(), PvField::Scalar(ScalarValue::Int(index))));
    value_struct
        .fields
        .push(("choices".into(), PvField::ScalarArray(choices)));

    pv.fields
        .push(("value".into(), PvField::Structure(value_struct)));
    pv.fields
        .push(("alarm".into(), PvField::Structure(build_alarm(snapshot))));
    pv.fields.push((
        "timeStamp".into(),
        PvField::Structure(build_timestamp(snapshot.timestamp, snapshot.user_tag)),
    ));
    // BR-R22: trailing `display.description` is part of pvxs's
    // QSRV NTEnum shape; populate from the DESC field when
    // available, otherwise emit an empty string so the field
    // is present (pvxs always emits the leaf).
    let mut display = PvStructure::new("");
    display.fields.push((
        "description".into(),
        PvField::Scalar(ScalarValue::String(
            snapshot
                .display
                .as_ref()
                .map(|d| d.description.clone())
                .unwrap_or_default(),
        )),
    ));
    pv.fields
        .push(("display".into(), PvField::Structure(display)));

    pv
}

/// Convert a Snapshot into an NTScalarArray PvStructure.
///
/// Structure ID: `epics:nt/NTScalarArray:1.0`
/// Fields: value[], alarm, timeStamp, display, control, valueAlarm.
///
/// BR-R12: pvxs builds NTScalarArray with the *same* `NTScalar` builder as
/// the scalar case — `value.isarray()` only flips the struct id — so a
/// numeric array carries `control` and `valueAlarm` just like a scalar
/// (pvxs `src/nt.cpp:44-112`, confirmed by `test/testqsingle.cpp:354-397`).
pub fn snapshot_to_nt_scalar_array(snapshot: &Snapshot) -> PvStructure {
    let mut pv = PvStructure::new("epics:nt/NTScalarArray:1.0");

    // BR-R12: array metadata limits take the element scalar type.
    let scalar_type = value_scalar_type(&snapshot.value);
    let numeric = is_numeric_scalar(scalar_type);

    // value (array)
    pv.fields
        .push(("value".into(), epics_to_pv_field(&snapshot.value)));

    // alarm
    pv.fields
        .push(("alarm".into(), PvField::Structure(build_alarm(snapshot))));

    // timeStamp
    pv.fields.push((
        "timeStamp".into(),
        PvField::Structure(build_timestamp(snapshot.timestamp, snapshot.user_tag)),
    ));

    // display
    if let Some(ref disp) = snapshot.display {
        pv.fields.push((
            "display".into(),
            PvField::Structure(build_display(disp, scalar_type, numeric)),
        ));
    }

    // control — numeric arrays only (pvxs src/nt.cpp:87).
    if numeric {
        if let Some(ref ctrl) = snapshot.control {
            pv.fields.push((
                "control".into(),
                PvField::Structure(build_control(ctrl, scalar_type)),
            ));
        }
    }

    // valueAlarm — numeric arrays only (pvxs src/nt.cpp:97).
    if numeric {
        if let Some(ref disp) = snapshot.display {
            pv.fields.push((
                "valueAlarm".into(),
                PvField::Structure(build_value_alarm(disp, scalar_type)),
            ));
        }
    }

    pv
}

/// Convert a Snapshot to the appropriate NormativeType based on NtType.
pub fn snapshot_to_pv_structure(snapshot: &Snapshot, nt_type: NtType) -> PvStructure {
    match nt_type {
        NtType::Scalar => snapshot_to_nt_scalar(snapshot),
        NtType::Enum => snapshot_to_nt_enum(snapshot),
        NtType::ScalarArray => snapshot_to_nt_scalar_array(snapshot),
    }
}

// ---------------------------------------------------------------------------
// PvStructure → EpicsValue extraction (for put path)
// ---------------------------------------------------------------------------

/// Extract the primary value from a PvStructure (for put operations).
///
/// For NTScalar: extracts "value" scalar field.
/// For NTEnum: extracts "value.index" as Enum.
/// For NTScalarArray: extracts "value" array.
pub fn pv_structure_to_epics(pv: &PvStructure) -> Option<EpicsValue> {
    let field = pv.get_field("value")?;
    match field {
        PvField::Scalar(sv) => Some(crate::convert::scalar_to_epics(sv)),
        PvField::ScalarArray(_) | PvField::ScalarArrayTyped(_) => {
            crate::convert::pv_field_to_epics(field)
        }
        PvField::Structure(s) => {
            // NTEnum: value is a sub-structure with "index" field
            if let Some(PvField::Scalar(sv)) = s.get_field("index") {
                let idx = crate::convert::scalar_to_epics(sv);
                match idx {
                    EpicsValue::Enum(v) => Some(EpicsValue::Enum(v)),
                    other => Some(EpicsValue::Enum(
                        other.to_f64().map(|f| f as u16).unwrap_or(0),
                    )),
                }
            } else {
                None
            }
        }
        // Other composite shapes are not (yet) represented as a single
        // EpicsValue. Handled out-of-line by the qsrv group/native source.
        PvField::StructureArray(_)
        | PvField::Union { .. }
        | PvField::UnionArray(_)
        | PvField::Variant(_)
        | PvField::VariantArray(_)
        | PvField::Null => None,
    }
}

// ---------------------------------------------------------------------------
// pvRequest field selection
// ---------------------------------------------------------------------------

/// Filter a PvStructure to only include fields requested in pvRequest.
///
/// pvRequest is a PvStructure describing which fields the client wants.
/// If pvRequest has a "field" sub-structure, only those named fields are kept.
/// If pvRequest is empty or has no "field", return the full structure.
///
/// **Nested filtering**: when a requested field is itself a non-empty
/// structure in the request, it acts as a sub-spec that recursively
/// filters the corresponding PvStructure field. An empty structure
/// in the request means "include this field entirely".
///
/// Example:
/// ```text
/// request: { field: { value: {}, alarm: { severity: {} } } }
/// pv:      { value: 42, alarm: {severity: 0, status: 0, message: ""}, timeStamp: {...} }
/// result:  { value: 42, alarm: { severity: 0 } }
/// ```
///
/// Corresponds to C++ QSRV's pvRequest mask handling.
pub fn filter_by_request(pv: &PvStructure, request: &PvStructure) -> PvStructure {
    // Look for "field" sub-structure in request
    let field_spec = match request.get_field("field") {
        Some(PvField::Structure(s)) => s,
        _ => return pv.clone(), // No field filter, return everything
    };

    filter_by_spec(pv, field_spec)
}

/// Recursively filter `pv` by the given field spec.
///
/// The spec is a PvStructure where each child indicates which sub-field
/// to keep. An empty child structure means "include this field entirely".
/// A non-empty child structure recursively filters that sub-field.
fn filter_by_spec(pv: &PvStructure, spec: &PvStructure) -> PvStructure {
    // Empty spec → return everything (passthrough)
    if spec.fields.is_empty() {
        return pv.clone();
    }

    let mut result = PvStructure::new(&pv.struct_id);
    for (name, value) in &pv.fields {
        let sub_spec = match spec.get_field(name) {
            Some(s) => s,
            None => continue, // Field not in spec, skip
        };

        match (sub_spec, value) {
            // Both are structures: recurse
            (PvField::Structure(s_spec), PvField::Structure(s_val)) => {
                result.fields.push((
                    name.clone(),
                    PvField::Structure(filter_by_spec(s_val, s_spec)),
                ));
            }
            // Spec is structure but value is scalar/array: include as-is
            // (the spec just selects the field, doesn't restructure it)
            (_, _) => {
                result.fields.push((name.clone(), value.clone()));
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// FieldDesc builders (type introspection, no values)
// ---------------------------------------------------------------------------

/// Build a PVA FieldDesc for an NTScalar with the given scalar type.
///
/// BR-R12: `display`/`control`/`valueAlarm` limits take `scalar_type`, and
/// `control`/`valueAlarm` are omitted for non-numeric (string) values —
/// matching pvxs `NTScalar::build` (`src/nt.cpp:37-114`).
pub fn build_nt_scalar_desc(scalar_type: ScalarType) -> FieldDesc {
    let numeric = is_numeric_scalar(scalar_type);
    let mut fields = vec![
        ("value".into(), FieldDesc::Scalar(scalar_type)),
        ("alarm".into(), alarm_desc()),
        ("timeStamp".into(), timestamp_desc()),
        ("display".into(), display_desc(scalar_type, numeric)),
    ];
    if numeric {
        fields.push(("control".into(), control_desc(scalar_type)));
        fields.push(("valueAlarm".into(), value_alarm_desc(scalar_type)));
    }
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields,
    }
}

/// Build a PVA FieldDesc for an NTEnum.
///
/// BR-R22: `value.index` is `Int` (matches pvxs QSRV
/// `testqsingle.cpp:174` `value.index int32_t`); the shape
/// includes a trailing `display.description` field.
pub fn build_nt_enum_desc() -> FieldDesc {
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

/// Build a PVA FieldDesc for an NTScalarArray with the given element type.
///
/// BR-R12: pvxs reuses the `NTScalar` builder for arrays, so a numeric
/// array descriptor carries `control` and `valueAlarm` with element-typed
/// limits (pvxs `src/nt.cpp:44-112`, `test/testqsingle.cpp:354-397`).
pub fn build_nt_scalar_array_desc(element_type: ScalarType) -> FieldDesc {
    let numeric = is_numeric_scalar(element_type);
    let mut fields = vec![
        ("value".into(), FieldDesc::ScalarArray(element_type)),
        ("alarm".into(), alarm_desc()),
        ("timeStamp".into(), timestamp_desc()),
        ("display".into(), display_desc(element_type, numeric)),
    ];
    if numeric {
        fields.push(("control".into(), control_desc(element_type)));
        fields.push(("valueAlarm".into(), value_alarm_desc(element_type)));
    }
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalarArray:1.0".into(),
        fields,
    }
}

/// Build the appropriate FieldDesc based on NtType and scalar type.
pub fn build_field_desc_for_nt(nt_type: NtType, scalar_type: ScalarType) -> FieldDesc {
    match nt_type {
        NtType::Scalar => build_nt_scalar_desc(scalar_type),
        NtType::Enum => build_nt_enum_desc(),
        NtType::ScalarArray => build_nt_scalar_array_desc(scalar_type),
    }
}

// ---------------------------------------------------------------------------
// Helper builders
// ---------------------------------------------------------------------------

fn build_alarm(snapshot: &Snapshot) -> PvStructure {
    let mut alarm = PvStructure::new("alarm_t");
    alarm.fields.push((
        "severity".into(),
        PvField::Scalar(ScalarValue::Int(snapshot.alarm.severity as i32)),
    ));
    // BR-R62: PVA alarm.status is the status CLASS, and alarm.message is
    // the condition string (pvxs iocsource.cpp:187-236) — not the raw
    // condition code / severity name.
    alarm.fields.push((
        "status".into(),
        PvField::Scalar(ScalarValue::Int(alarm_status_class(snapshot.alarm.status))),
    ));
    alarm.fields.push((
        "message".into(),
        PvField::Scalar(ScalarValue::String(
            alarm_condition_string(snapshot.alarm.status).to_string(),
        )),
    ));
    alarm
}

/// Build an alarm overlay that escalates severity/status without losing the
/// underlying record context. Used when a structural mismatch (e.g. empty
/// array fed to NTScalar) makes the value field unreliable.
fn build_alarm_overlay(snapshot: &Snapshot, severity: u16, status: u16) -> PvStructure {
    let eff_severity = snapshot.alarm.severity.max(severity);
    let eff_status = if snapshot.alarm.status == 0 {
        status
    } else {
        snapshot.alarm.status
    };
    let mut alarm = PvStructure::new("alarm_t");
    alarm.fields.push((
        "severity".into(),
        PvField::Scalar(ScalarValue::Int(eff_severity as i32)),
    ));
    // BR-R62: status CLASS + condition-string message (pvxs
    // iocsource.cpp:187-236), using the escalated `eff_status`.
    alarm.fields.push((
        "status".into(),
        PvField::Scalar(ScalarValue::Int(alarm_status_class(eff_status))),
    ));
    alarm.fields.push((
        "message".into(),
        PvField::Scalar(ScalarValue::String(
            alarm_condition_string(eff_status).to_string(),
        )),
    ));
    alarm
}

/// True when `value` is an array variant containing zero elements.
fn is_empty_array(value: &EpicsValue) -> bool {
    matches!(
        value,
        EpicsValue::ShortArray(a) if a.is_empty()
    ) || matches!(
        value,
        EpicsValue::FloatArray(a) if a.is_empty()
    ) || matches!(
        value,
        EpicsValue::EnumArray(a) if a.is_empty()
    ) || matches!(
        value,
        EpicsValue::DoubleArray(a) if a.is_empty()
    ) || matches!(
        value,
        EpicsValue::LongArray(a) if a.is_empty()
    ) || matches!(
        value,
        EpicsValue::CharArray(a) if a.is_empty()
    ) || matches!(
        value,
        EpicsValue::StringArray(a) if a.is_empty()
    )
}

fn build_timestamp(time: SystemTime, user_tag: i32) -> PvStructure {
    let mut ts = PvStructure::new("time_t");
    let (secs, nanos) = match time.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos() as i32),
        Err(_) => (0, 0),
    };
    // PVA Normative Types define secondsPastEpoch as POSIX/UNIX epoch
    // (pvxs iocsource.cpp:240 adds POSIX_TIME_AT_EPICS_EPOCH to convert
    // from internal EPICS epoch). Rust SystemTime is already UNIX-based,
    // so no conversion is needed here.
    ts.fields.push((
        "secondsPastEpoch".into(),
        PvField::Scalar(ScalarValue::Long(secs)),
    ));
    ts.fields.push((
        "nanoseconds".into(),
        PvField::Scalar(ScalarValue::Int(nanos)),
    ));
    ts.fields.push((
        "userTag".into(),
        PvField::Scalar(ScalarValue::Int(user_tag)),
    ));
    ts
}

/// Build the `enum_t` sub-structure for `display.form`.
///
/// BR-R12: pvxs models `display.form` as an `enum_t` (`{int32 index,
/// string[] choices}`), not a scalar `int`. `index` is the `Q:form` info
/// tag value; `choices` is the fixed seven-entry menu.
/// Mirrors pvxs `src/nt.cpp:71-74` and `ioc/iocsource.cpp:42-62`.
fn build_form(form: i16) -> PvStructure {
    let mut f = PvStructure::new("enum_t");
    f.fields.push((
        "index".into(),
        PvField::Scalar(ScalarValue::Int(form as i32)),
    ));
    f.fields.push((
        "choices".into(),
        PvField::ScalarArray(
            FORM_CHOICES
                .iter()
                .map(|s| ScalarValue::String((*s).to_string()))
                .collect(),
        ),
    ));
    f
}

/// Build the `display` sub-structure.
///
/// BR-R12: for numeric values pvxs emits `{limitLow, limitHigh,
/// description, units, precision, form}` with limits typed as the value's
/// scalar type and `form` as an `enum_t`; for non-numeric (string) values
/// only `{description, units}` is emitted (pvxs `src/nt.cpp:58-85`).
fn build_display(disp: &DisplayInfo, scalar_type: ScalarType, numeric: bool) -> PvStructure {
    let mut d = PvStructure::new("display_t");
    if numeric {
        d.fields.push((
            "limitLow".into(),
            PvField::Scalar(limit_scalar(scalar_type, disp.lower_disp_limit)),
        ));
        d.fields.push((
            "limitHigh".into(),
            PvField::Scalar(limit_scalar(scalar_type, disp.upper_disp_limit)),
        ));
    }
    d.fields.push((
        "description".into(),
        PvField::Scalar(ScalarValue::String(disp.description.clone())),
    ));
    d.fields.push((
        "units".into(),
        PvField::Scalar(ScalarValue::String(disp.units.clone())),
    ));
    if numeric {
        d.fields.push((
            "precision".into(),
            PvField::Scalar(ScalarValue::Int(disp.precision as i32)),
        ));
        d.fields
            .push(("form".into(), PvField::Structure(build_form(disp.form))));
    }
    d
}

fn build_control(ctrl: &ControlInfo, scalar_type: ScalarType) -> PvStructure {
    let mut c = PvStructure::new("control_t");
    c.fields.push((
        "limitLow".into(),
        PvField::Scalar(limit_scalar(scalar_type, ctrl.lower_ctrl_limit)),
    ));
    c.fields.push((
        "limitHigh".into(),
        PvField::Scalar(limit_scalar(scalar_type, ctrl.upper_ctrl_limit)),
    ));
    c.fields.push((
        "minStep".into(),
        PvField::Scalar(limit_scalar(scalar_type, 0.0)),
    ));
    c
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

/// FieldDesc for `display.form` — an `enum_t` (`{int32 index, string[]
/// choices}`). Mirrors pvxs `src/nt.cpp:71-74`.
fn form_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "enum_t".into(),
        fields: vec![
            ("index".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("choices".into(), FieldDesc::ScalarArray(ScalarType::String)),
        ],
    }
}

fn display_desc(scalar_type: ScalarType, numeric: bool) -> FieldDesc {
    let mut fields: Vec<(String, FieldDesc)> = Vec::new();
    if numeric {
        fields.push(("limitLow".into(), FieldDesc::Scalar(scalar_type)));
        fields.push(("limitHigh".into(), FieldDesc::Scalar(scalar_type)));
    }
    fields.push(("description".into(), FieldDesc::Scalar(ScalarType::String)));
    fields.push(("units".into(), FieldDesc::Scalar(ScalarType::String)));
    if numeric {
        fields.push(("precision".into(), FieldDesc::Scalar(ScalarType::Int)));
        fields.push(("form".into(), form_desc()));
    }
    FieldDesc::Structure {
        struct_id: "display_t".into(),
        fields,
    }
}

fn control_desc(scalar_type: ScalarType) -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "control_t".into(),
        fields: vec![
            ("limitLow".into(), FieldDesc::Scalar(scalar_type)),
            ("limitHigh".into(), FieldDesc::Scalar(scalar_type)),
            ("minStep".into(), FieldDesc::Scalar(scalar_type)),
        ],
    }
}

/// Build the `valueAlarm` sub-structure.
///
/// BR-R12: pvxs `valueAlarm` carries the full field set — `active` (bool),
/// the four alarm/warning limits typed as the value's scalar type, the
/// four `*Severity` fields (int32), and `hysteresis` (float64). The four
/// `*Severity` fields and `active`/`hysteresis` are not represented in the
/// EPICS `DisplayInfo` metadata, so they default to 0 / false / 0.0 — the
/// same values pvxs emits when QSRV does not populate them
/// (`test/testqsingle.cpp:116-127`). Mirrors pvxs `src/nt.cpp:97-112`.
fn build_value_alarm(disp: &DisplayInfo, scalar_type: ScalarType) -> PvStructure {
    let mut va = PvStructure::new("valueAlarm_t");
    va.fields.push((
        "active".into(),
        PvField::Scalar(ScalarValue::Boolean(false)),
    ));
    va.fields.push((
        "lowAlarmLimit".into(),
        PvField::Scalar(limit_scalar(scalar_type, disp.lower_alarm_limit)),
    ));
    va.fields.push((
        "lowWarningLimit".into(),
        PvField::Scalar(limit_scalar(scalar_type, disp.lower_warning_limit)),
    ));
    va.fields.push((
        "highWarningLimit".into(),
        PvField::Scalar(limit_scalar(scalar_type, disp.upper_warning_limit)),
    ));
    va.fields.push((
        "highAlarmLimit".into(),
        PvField::Scalar(limit_scalar(scalar_type, disp.upper_alarm_limit)),
    ));
    va.fields.push((
        "lowAlarmSeverity".into(),
        PvField::Scalar(ScalarValue::Int(0)),
    ));
    va.fields.push((
        "lowWarningSeverity".into(),
        PvField::Scalar(ScalarValue::Int(0)),
    ));
    va.fields.push((
        "highWarningSeverity".into(),
        PvField::Scalar(ScalarValue::Int(0)),
    ));
    va.fields.push((
        "highAlarmSeverity".into(),
        PvField::Scalar(ScalarValue::Int(0)),
    ));
    va.fields.push((
        "hysteresis".into(),
        PvField::Scalar(ScalarValue::Double(0.0)),
    ));
    va
}

fn value_alarm_desc(scalar_type: ScalarType) -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "valueAlarm_t".into(),
        fields: vec![
            ("active".into(), FieldDesc::Scalar(ScalarType::Boolean)),
            ("lowAlarmLimit".into(), FieldDesc::Scalar(scalar_type)),
            ("lowWarningLimit".into(), FieldDesc::Scalar(scalar_type)),
            ("highWarningLimit".into(), FieldDesc::Scalar(scalar_type)),
            ("highAlarmLimit".into(), FieldDesc::Scalar(scalar_type)),
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
            ("hysteresis".into(), FieldDesc::Scalar(ScalarType::Double)),
        ],
    }
}

/// BR-R62: map a raw EPICS `epicsAlarmCondition` (0–21, `alarm.h`) to the
/// PVA `alarm_t.status` **status class**. PVA carries the alarm *class*
/// (NONE/DEVICE/DRIVER/RECORD/DB/UNDEFINED), not the raw DB condition
/// code — mirrors pvxs `ioc/iocsource.cpp:187-223`.
pub(crate) fn alarm_status_class(condition: u16) -> i32 {
    use epics_base_rs::server::recgbl::alarm_status as a;
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

/// BR-R62: the EPICS alarm **condition string** for a raw
/// `epicsAlarmCondition` (`epicsAlarmConditionStrings`,
/// `libcom/src/misc/alarmString.c:27-50`), or `""` for `NO_ALARM` /
/// out-of-range. pvxs uses this for `alarm.message`
/// (`ioc/iocsource.cpp:225-236`).
pub(crate) fn alarm_condition_string(condition: u16) -> &'static str {
    use epics_base_rs::server::recgbl::alarm_status as a;
    match condition {
        a::NO_ALARM => "",
        a::READ_ALARM => "READ",
        a::WRITE_ALARM => "WRITE",
        a::HIHI_ALARM => "HIHI",
        a::HIGH_ALARM => "HIGH",
        a::LOLO_ALARM => "LOLO",
        a::LOW_ALARM => "LOW",
        a::STATE_ALARM => "STATE",
        a::COS_ALARM => "COS",
        a::COMM_ALARM => "COMM",
        a::TIMEOUT_ALARM => "TIMEOUT",
        a::HW_LIMIT_ALARM => "HWLIMIT",
        a::CALC_ALARM => "CALC",
        a::SCAN_ALARM => "SCAN",
        a::LINK_ALARM => "LINK",
        a::SOFT_ALARM => "SOFT",
        a::BAD_SUB_ALARM => "BAD_SUB",
        a::UDF_ALARM => "UDF",
        a::DISABLE_ALARM => "DISABLE",
        a::SIMM_ALARM => "SIMM",
        a::READ_ACCESS_ALARM => "READ_ACCESS",
        a::WRITE_ACCESS_ALARM => "WRITE_ACCESS",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_base_rs::server::snapshot::{EnumInfo, Snapshot};

    fn test_snapshot(value: EpicsValue) -> Snapshot {
        let mut snap = Snapshot::new(value, 0, 0, UNIX_EPOCH);
        snap.display = Some(DisplayInfo {
            units: "degC".into(),
            precision: 3,
            upper_disp_limit: 100.0,
            lower_disp_limit: 0.0,
            upper_alarm_limit: 90.0,
            upper_warning_limit: 80.0,
            lower_warning_limit: 10.0,
            lower_alarm_limit: 5.0,
            ..Default::default()
        });
        snap.control = Some(ControlInfo {
            upper_ctrl_limit: 100.0,
            lower_ctrl_limit: 0.0,
        });
        snap
    }

    fn alarm_scalar_int(s: &PvStructure, name: &str) -> i32 {
        match s.fields.iter().find(|(n, _)| n == name).map(|(_, f)| f) {
            Some(PvField::Scalar(ScalarValue::Int(v))) => *v,
            other => panic!("expected Int field {name}, got {other:?}"),
        }
    }

    fn alarm_scalar_str(s: &PvStructure, name: &str) -> String {
        match s.fields.iter().find(|(n, _)| n == name).map(|(_, f)| f) {
            Some(PvField::Scalar(ScalarValue::String(v))) => v.clone(),
            other => panic!("expected String field {name}, got {other:?}"),
        }
    }

    #[test]
    fn br_r62_status_class_mapping() {
        // Raw epicsAlarmCondition -> PVA status class (pvxs iocsource.cpp).
        use epics_base_rs::server::recgbl::alarm_status as a;
        assert_eq!(alarm_status_class(a::NO_ALARM), 0); // NONE
        assert_eq!(alarm_status_class(a::HIGH_ALARM), 1); // DEVICE
        assert_eq!(alarm_status_class(a::HW_LIMIT_ALARM), 1); // DEVICE
        assert_eq!(alarm_status_class(a::COMM_ALARM), 2); // DRIVER
        assert_eq!(alarm_status_class(a::UDF_ALARM), 2); // DRIVER
        assert_eq!(alarm_status_class(a::LINK_ALARM), 3); // RECORD
        assert_eq!(alarm_status_class(a::SCAN_ALARM), 3); // RECORD
        assert_eq!(alarm_status_class(a::WRITE_ACCESS_ALARM), 4); // DB
        assert_eq!(alarm_status_class(a::SIMM_ALARM), 4); // DB
        assert_eq!(alarm_status_class(999), 6); // UNDEFINED (out of range)
    }

    #[test]
    fn br_r62_condition_string() {
        // alarm.message is the condition string, "" for NO_ALARM/out-of-range.
        use epics_base_rs::server::recgbl::alarm_status as a;
        assert_eq!(alarm_condition_string(a::NO_ALARM), "");
        assert_eq!(alarm_condition_string(a::HIGH_ALARM), "HIGH");
        assert_eq!(alarm_condition_string(a::LINK_ALARM), "LINK");
        assert_eq!(alarm_condition_string(a::HW_LIMIT_ALARM), "HWLIMIT");
        assert_eq!(alarm_condition_string(a::UDF_ALARM), "UDF");
        assert_eq!(alarm_condition_string(999), "");
    }

    #[test]
    fn br_r62_build_alarm_emits_class_and_condition_message() {
        // LINK_ALARM (raw 14) must emit status class RECORD (3) and the
        // condition string "LINK" — not raw 14 / the severity name.
        use epics_base_rs::server::recgbl::alarm_status as a;
        let snap = Snapshot::new(EpicsValue::Double(1.0), a::LINK_ALARM, 2, UNIX_EPOCH);
        let alarm = build_alarm(&snap);
        assert_eq!(alarm_scalar_int(&alarm, "severity"), 2); // raw severity
        assert_eq!(alarm_scalar_int(&alarm, "status"), 3); // RECORD, not 14
        assert_eq!(alarm_scalar_str(&alarm, "message"), "LINK");
    }

    #[test]
    fn br_r62_build_alarm_no_alarm_has_empty_message() {
        let snap = Snapshot::new(EpicsValue::Double(1.0), 0, 0, UNIX_EPOCH);
        let alarm = build_alarm(&snap);
        assert_eq!(alarm_scalar_int(&alarm, "status"), 0); // NONE
        assert_eq!(alarm_scalar_str(&alarm, "message"), "");
    }

    #[test]
    fn nt_scalar_structure() {
        let snap = test_snapshot(EpicsValue::Double(42.5));
        let pv = snapshot_to_nt_scalar(&snap);

        assert_eq!(pv.struct_id, "epics:nt/NTScalar:1.0");
        assert_eq!(pv.get_value(), Some(&ScalarValue::Double(42.5)));
        assert!(pv.get_alarm().is_some());
        assert!(pv.get_timestamp().is_some());
        assert!(pv.get_field("display").is_some());
        assert!(pv.get_field("control").is_some());
        // valueAlarm with alarm thresholds
        let va = pv.get_field("valueAlarm");
        assert!(va.is_some());
        if let Some(PvField::Structure(va_struct)) = va {
            assert!(va_struct.get_field("lowAlarmLimit").is_some());
            assert!(va_struct.get_field("highAlarmLimit").is_some());
            assert!(va_struct.get_field("lowWarningLimit").is_some());
            assert!(va_struct.get_field("highWarningLimit").is_some());
        } else {
            panic!("expected valueAlarm structure");
        }
    }

    #[test]
    fn nt_scalar_empty_array_marks_invalid_udf() {
        // F6: when an empty array reaches NTScalar conversion, value cannot
        // be recovered — alarm must escalate to INVALID severity / UDF status
        // so clients don't read the placeholder zero as a valid sample.
        // BR-R62: alarm.status is the PVA status CLASS — UDF maps to DRIVER
        // (2) — and alarm.message is the condition string "UDF".
        let snap = test_snapshot(EpicsValue::DoubleArray(vec![]));
        let pv = snapshot_to_nt_scalar(&snap);

        if let Some(PvField::Structure(alarm)) = pv.get_field("alarm") {
            let sev = alarm.get_field("severity");
            let st = alarm.get_field("status");
            let msg = alarm.get_field("message");
            assert!(matches!(sev, Some(PvField::Scalar(ScalarValue::Int(3)))));
            assert!(matches!(st, Some(PvField::Scalar(ScalarValue::Int(2)))));
            assert!(matches!(msg, Some(PvField::Scalar(ScalarValue::String(s))) if s == "UDF"));
        } else {
            panic!("expected alarm structure");
        }
    }

    #[test]
    fn nt_scalar_non_empty_keeps_original_alarm() {
        // Sanity: non-empty array (or scalar) does NOT trigger the overlay.
        let snap = test_snapshot(EpicsValue::Double(1.0));
        let pv = snapshot_to_nt_scalar(&snap);
        if let Some(PvField::Structure(alarm)) = pv.get_field("alarm") {
            let sev = alarm.get_field("severity");
            assert!(matches!(sev, Some(PvField::Scalar(ScalarValue::Int(0)))));
        } else {
            panic!("expected alarm structure");
        }
    }

    /// BR-R22: pvxs QSRV NTEnum uses int32_t index +
    /// `display.description` (testqsingle.cpp:174).
    #[test]
    fn nt_enum_structure() {
        let mut snap = Snapshot::new(EpicsValue::Enum(1), 0, 0, UNIX_EPOCH);
        snap.enums = Some(EnumInfo {
            strings: vec!["Off".into(), "On".into()],
        });
        let pv = snapshot_to_nt_enum(&snap);

        assert_eq!(pv.struct_id, "epics:nt/NTEnum:1.0");
        if let Some(PvField::Structure(val)) = pv.get_field("value") {
            if let Some(PvField::Scalar(ScalarValue::Int(idx))) = val.get_field("index") {
                assert_eq!(*idx, 1);
            } else {
                panic!(
                    "expected int32_t index scalar, got {:?}",
                    val.get_field("index")
                );
            }
            if let Some(PvField::ScalarArray(choices)) = val.get_field("choices") {
                assert_eq!(choices.len(), 2);
            } else {
                panic!("expected choices array");
            }
        } else {
            panic!("expected value structure");
        }

        // BR-R22: display.description is part of pvxs's NTEnum shape.
        if let Some(PvField::Structure(d)) = pv.get_field("display") {
            assert!(
                matches!(
                    d.get_field("description"),
                    Some(PvField::Scalar(ScalarValue::String(_)))
                ),
                "expected display.description string"
            );
        } else {
            panic!("expected display structure");
        }
    }

    #[test]
    fn nt_scalar_array_structure() {
        let snap = test_snapshot(EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]));
        let pv = snapshot_to_nt_scalar_array(&snap);

        assert_eq!(pv.struct_id, "epics:nt/NTScalarArray:1.0");
        if let Some(PvField::ScalarArray(arr)) = pv.get_field("value") {
            assert_eq!(arr.len(), 3);
        } else {
            panic!("expected value array");
        }
    }

    #[test]
    fn put_roundtrip_scalar() {
        let snap = test_snapshot(EpicsValue::Double(99.0));
        let pv = snapshot_to_nt_scalar(&snap);
        let back = pv_structure_to_epics(&pv).unwrap();
        assert_eq!(back, EpicsValue::Double(99.0));
    }

    #[test]
    fn put_roundtrip_enum() {
        let mut snap = Snapshot::new(EpicsValue::Enum(2), 0, 0, UNIX_EPOCH);
        snap.enums = Some(EnumInfo {
            strings: vec!["A".into(), "B".into(), "C".into()],
        });
        let pv = snapshot_to_nt_enum(&snap);
        let back = pv_structure_to_epics(&pv).unwrap();
        assert_eq!(back, EpicsValue::Enum(2));
    }

    #[test]
    fn nt_type_from_record_type() {
        assert_eq!(NtType::from_record_type("ai"), NtType::Scalar);
        assert_eq!(NtType::from_record_type("bi"), NtType::Enum);
        assert_eq!(NtType::from_record_type("waveform"), NtType::ScalarArray);
        assert_eq!(NtType::from_record_type("calc"), NtType::Scalar);
        assert_eq!(NtType::from_record_type("mbbi"), NtType::Enum);
    }

    #[test]
    fn field_desc_nt_scalar() {
        let desc = build_nt_scalar_desc(ScalarType::Double);
        assert_eq!(desc.value_scalar_type(), Some(ScalarType::Double));
        assert_eq!(desc.field_count(), 6); // value, alarm, timeStamp, display, control, valueAlarm
    }

    #[test]
    fn filter_by_request_empty() {
        let snap = test_snapshot(EpicsValue::Double(1.0));
        let pv = snapshot_to_nt_scalar(&snap);

        // Empty request → return everything
        let req = PvStructure::new("");
        let filtered = filter_by_request(&pv, &req);
        assert_eq!(filtered.fields.len(), pv.fields.len());
    }

    #[test]
    fn filter_by_request_value_only() {
        let snap = test_snapshot(EpicsValue::Double(1.0));
        let pv = snapshot_to_nt_scalar(&snap);

        // Request only "value" field
        let mut field_spec = PvStructure::new("");
        field_spec
            .fields
            .push(("value".into(), PvField::Structure(PvStructure::new(""))));
        let mut req = PvStructure::new("");
        req.fields
            .push(("field".into(), PvField::Structure(field_spec)));

        let filtered = filter_by_request(&pv, &req);
        assert_eq!(filtered.fields.len(), 1);
        assert_eq!(filtered.fields[0].0, "value");
    }

    #[test]
    fn filter_by_request_multiple_fields() {
        let snap = test_snapshot(EpicsValue::Double(1.0));
        let pv = snapshot_to_nt_scalar(&snap);

        let mut field_spec = PvStructure::new("");
        field_spec
            .fields
            .push(("value".into(), PvField::Structure(PvStructure::new(""))));
        field_spec
            .fields
            .push(("alarm".into(), PvField::Structure(PvStructure::new(""))));
        let mut req = PvStructure::new("");
        req.fields
            .push(("field".into(), PvField::Structure(field_spec)));

        let filtered = filter_by_request(&pv, &req);
        assert_eq!(filtered.fields.len(), 2);
    }

    #[test]
    fn filter_by_request_nested_subfield() {
        let snap = test_snapshot(EpicsValue::Double(1.0));
        let pv = snapshot_to_nt_scalar(&snap);

        // Build request: {field: {alarm: {severity: {}}}}
        // — only return alarm.severity, not other alarm fields
        let mut alarm_spec = PvStructure::new("");
        alarm_spec
            .fields
            .push(("severity".into(), PvField::Structure(PvStructure::new(""))));

        let mut field_spec = PvStructure::new("");
        field_spec
            .fields
            .push(("alarm".into(), PvField::Structure(alarm_spec)));

        let mut req = PvStructure::new("");
        req.fields
            .push(("field".into(), PvField::Structure(field_spec)));

        let filtered = filter_by_request(&pv, &req);
        assert_eq!(filtered.fields.len(), 1);
        assert_eq!(filtered.fields[0].0, "alarm");

        // Verify alarm only has "severity" sub-field, not "status" or "message"
        if let PvField::Structure(alarm) = &filtered.fields[0].1 {
            assert_eq!(alarm.fields.len(), 1);
            assert_eq!(alarm.fields[0].0, "severity");
        } else {
            panic!("expected alarm structure");
        }
    }

    #[test]
    fn br_r12_array_metadata_shape_matches_pvxs() {
        // BR-R12: an int32 NTScalarArray must carry `control` and
        // `valueAlarm` (pvxs reuses the NTScalar builder for arrays —
        // src/nt.cpp:44-112, test/testqsingle.cpp:354-397), the
        // display/control/valueAlarm limits must be typed as the element
        // scalar type (Int, not Double), and `display.form` must be an
        // `enum_t` sub-structure, not a scalar int.
        let snap = test_snapshot(EpicsValue::LongArray(vec![4, 5, 6, 7]));
        let pv = snapshot_to_nt_scalar_array(&snap);

        // control + valueAlarm present on the array
        let control = pv.get_field("control").expect("array must carry control");
        let value_alarm = pv
            .get_field("valueAlarm")
            .expect("array must carry valueAlarm");

        // display.limitLow typed as Int (element scalar type), not Double
        if let Some(PvField::Structure(disp)) = pv.get_field("display") {
            assert!(
                matches!(
                    disp.get_field("limitLow"),
                    Some(PvField::Scalar(ScalarValue::Int(_)))
                ),
                "display.limitLow must be Int for an int32 array"
            );
            // display.form must be an enum_t structure with index + choices
            match disp.get_field("form") {
                Some(PvField::Structure(form)) => {
                    assert_eq!(form.struct_id, "enum_t");
                    assert!(matches!(
                        form.get_field("index"),
                        Some(PvField::Scalar(ScalarValue::Int(_)))
                    ));
                    if let Some(PvField::ScalarArray(choices)) = form.get_field("choices") {
                        assert_eq!(choices.len(), 7, "form.choices is the fixed 7-entry menu");
                    } else {
                        panic!("display.form.choices must be a string array");
                    }
                }
                _ => panic!("display.form must be an enum_t structure"),
            }
        } else {
            panic!("expected display structure");
        }

        // control.limitLow typed as Int
        if let PvField::Structure(c) = control {
            assert!(matches!(
                c.get_field("limitLow"),
                Some(PvField::Scalar(ScalarValue::Int(_)))
            ));
        } else {
            panic!("expected control structure");
        }

        // valueAlarm full field set: active, 4 limits (Int), 4 severities, hysteresis
        if let PvField::Structure(va) = value_alarm {
            assert!(matches!(
                va.get_field("active"),
                Some(PvField::Scalar(ScalarValue::Boolean(false)))
            ));
            assert!(matches!(
                va.get_field("lowAlarmLimit"),
                Some(PvField::Scalar(ScalarValue::Int(_)))
            ));
            assert!(va.get_field("lowAlarmSeverity").is_some());
            assert!(va.get_field("highAlarmSeverity").is_some());
            assert!(matches!(
                va.get_field("hysteresis"),
                Some(PvField::Scalar(ScalarValue::Double(_)))
            ));
        } else {
            panic!("expected valueAlarm structure");
        }

        // Descriptor must mirror the same shape.
        let desc = build_nt_scalar_array_desc(ScalarType::Int);
        if let FieldDesc::Structure { fields, .. } = &desc {
            let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
            assert!(names.contains(&"control"), "array desc must carry control");
            assert!(
                names.contains(&"valueAlarm"),
                "array desc must carry valueAlarm"
            );
        } else {
            panic!("expected structure descriptor");
        }
    }

    #[test]
    fn br_r12_string_value_omits_numeric_metadata() {
        // BR-R12: a non-numeric (string) NTScalar carries only
        // `display = {description, units}` — no limits, no form, no
        // control, no valueAlarm (pvxs src/nt.cpp:78-85).
        let snap = test_snapshot(EpicsValue::String("Analog input".into()));
        let pv = snapshot_to_nt_scalar(&snap);

        assert!(
            pv.get_field("control").is_none(),
            "string value must not carry control"
        );
        assert!(
            pv.get_field("valueAlarm").is_none(),
            "string value must not carry valueAlarm"
        );
        if let Some(PvField::Structure(disp)) = pv.get_field("display") {
            assert!(disp.get_field("limitLow").is_none());
            assert!(disp.get_field("form").is_none());
            assert!(disp.get_field("description").is_some());
            assert!(disp.get_field("units").is_some());
        } else {
            panic!("expected display structure");
        }

        let desc = build_nt_scalar_desc(ScalarType::String);
        if let FieldDesc::Structure { fields, .. } = &desc {
            let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
            assert!(!names.contains(&"control"));
            assert!(!names.contains(&"valueAlarm"));
        } else {
            panic!("expected structure descriptor");
        }
    }

    #[test]
    fn br_r13_uint64_array_qsrv_descriptor_uses_ulong() {
        // BR-R13: a `waveform` with `FTVL = UINT64` must be advertised
        // through QSRV as `ulong[]` with `uint64_t`-typed metadata limits,
        // matching pvxs test/testqsingle.cpp:530-546. On main there was no
        // `EpicsValue::UInt64Array`, so the value collapsed into
        // `DoubleArray` and the descriptor advertised `double[]`.
        let snap = test_snapshot(EpicsValue::UInt64Array(vec![
            11111111111111111,
            222222222222222,
        ]));
        let pv = snapshot_to_nt_scalar_array(&snap);

        // value is a ulong array
        match pv.get_field("value") {
            Some(PvField::ScalarArray(vs)) => {
                assert!(matches!(vs[0], ScalarValue::ULong(11111111111111111)));
            }
            other => panic!("expected ulong ScalarArray, got {other:?}"),
        }

        // display.limitLow typed as ULong, not Double
        if let Some(PvField::Structure(disp)) = pv.get_field("display") {
            assert!(
                matches!(
                    disp.get_field("limitLow"),
                    Some(PvField::Scalar(ScalarValue::ULong(_)))
                ),
                "uint64 array display.limitLow must be ulong"
            );
        } else {
            panic!("expected display structure");
        }

        // descriptor advertises ulong[] value and ulong limits
        let desc = build_nt_scalar_array_desc(ScalarType::ULong);
        if let FieldDesc::Structure { fields, .. } = &desc {
            let value_desc = fields.iter().find(|(n, _)| n == "value").map(|(_, d)| d);
            assert!(matches!(
                value_desc,
                Some(FieldDesc::ScalarArray(ScalarType::ULong))
            ));
            let control = fields.iter().find(|(n, _)| n == "control").map(|(_, d)| d);
            if let Some(FieldDesc::Structure { fields: cf, .. }) = control {
                assert!(matches!(
                    cf.iter().find(|(n, _)| n == "limitLow").map(|(_, d)| d),
                    Some(FieldDesc::Scalar(ScalarType::ULong))
                ));
            } else {
                panic!("expected control descriptor");
            }
        } else {
            panic!("expected structure descriptor");
        }
    }

    /// BR-R22: descriptor uses Int (not UShort) for `value.index`
    /// and includes a trailing `display.description` leaf.
    #[test]
    fn field_desc_nt_enum_index_int() {
        let desc = build_nt_enum_desc();
        if let FieldDesc::Structure { fields, .. } = &desc {
            // value.index = Int32
            if let Some((
                _,
                FieldDesc::Structure {
                    fields: val_fields, ..
                },
            )) = fields.iter().find(|(n, _)| n == "value")
            {
                let index_field = val_fields.iter().find(|(n, _)| n == "index");
                assert!(
                    matches!(index_field, Some((_, FieldDesc::Scalar(ScalarType::Int)))),
                    "expected NTEnum value.index Int32, got {index_field:?}"
                );
            } else {
                panic!("expected value structure");
            }
            // display.description = String
            if let Some((
                _,
                FieldDesc::Structure {
                    fields: disp_fields,
                    ..
                },
            )) = fields.iter().find(|(n, _)| n == "display")
            {
                let desc_field = disp_fields.iter().find(|(n, _)| n == "description");
                assert!(
                    matches!(desc_field, Some((_, FieldDesc::Scalar(ScalarType::String)))),
                    "expected display.description String"
                );
            } else {
                panic!("expected display sub-structure");
            }
        } else {
            panic!("expected NTEnum top-level structure");
        }
    }
}
