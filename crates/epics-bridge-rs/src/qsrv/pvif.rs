//! PVIF: PVData Interface — converts between EPICS record state and PVA structures.
//!
//! Corresponds to C++ QSRV's `pvif.h/pvif.cpp` (ScalarBuilder, etc.).

use std::time::{SystemTime, UNIX_EPOCH};

use epics_base_rs::server::snapshot::{ControlInfo, DisplayInfo, Snapshot};
use epics_base_rs::types::EpicsValue;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};

use super::convert::{epics_to_pv_field, epics_to_scalar};

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
        pv.fields
            .push(("display".into(), PvField::Structure(build_display(disp))));
    }

    // control
    if let Some(ref ctrl) = snapshot.control {
        pv.fields
            .push(("control".into(), PvField::Structure(build_control(ctrl))));
    }

    // valueAlarm (alarm thresholds from display limits)
    if let Some(ref disp) = snapshot.display {
        pv.fields.push((
            "valueAlarm".into(),
            PvField::Structure(build_value_alarm(disp)),
        ));
    }

    pv
}

/// Convert a Snapshot into an NTEnum PvStructure.
///
/// Structure ID: `epics:nt/NTEnum:1.0`
/// Fields: value{index, choices}, alarm, timeStamp
pub fn snapshot_to_nt_enum(snapshot: &Snapshot) -> PvStructure {
    let mut pv = PvStructure::new("epics:nt/NTEnum:1.0");

    // value sub-structure with index + choices
    let index = match &snapshot.value {
        EpicsValue::Enum(v) => *v,
        EpicsValue::Short(v) => *v as u16,
        other => other.to_f64().map(|f| f as u16).unwrap_or(0),
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
        .push(("index".into(), PvField::Scalar(ScalarValue::UShort(index))));
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

    pv
}

/// Convert a Snapshot into an NTScalarArray PvStructure.
///
/// Structure ID: `epics:nt/NTScalarArray:1.0`
/// Fields: value[], alarm, timeStamp, display (optional)
pub fn snapshot_to_nt_scalar_array(snapshot: &Snapshot) -> PvStructure {
    let mut pv = PvStructure::new("epics:nt/NTScalarArray:1.0");

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
        pv.fields
            .push(("display".into(), PvField::Structure(build_display(disp))));
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
        PvField::Scalar(sv) => Some(super::convert::scalar_to_epics(sv)),
        PvField::ScalarArray(_) | PvField::ScalarArrayTyped(_) => {
            super::convert::pv_field_to_epics(field)
        }
        PvField::Structure(s) => {
            // NTEnum: value is a sub-structure with "index" field
            if let Some(PvField::Scalar(sv)) = s.get_field("index") {
                let idx = super::convert::scalar_to_epics(sv);
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
pub fn build_nt_scalar_desc(scalar_type: ScalarType) -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields: vec![
            ("value".into(), FieldDesc::Scalar(scalar_type)),
            ("alarm".into(), alarm_desc()),
            ("timeStamp".into(), timestamp_desc()),
            ("display".into(), display_desc()),
            ("control".into(), control_desc()),
            ("valueAlarm".into(), value_alarm_desc()),
        ],
    }
}

/// Build a PVA FieldDesc for an NTEnum.
pub fn build_nt_enum_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTEnum:1.0".into(),
        fields: vec![
            (
                "value".into(),
                FieldDesc::Structure {
                    struct_id: "enum_t".into(),
                    fields: vec![
                        ("index".into(), FieldDesc::Scalar(ScalarType::UShort)),
                        ("choices".into(), FieldDesc::ScalarArray(ScalarType::String)),
                    ],
                },
            ),
            ("alarm".into(), alarm_desc()),
            ("timeStamp".into(), timestamp_desc()),
        ],
    }
}

/// Build a PVA FieldDesc for an NTScalarArray with the given element type.
pub fn build_nt_scalar_array_desc(element_type: ScalarType) -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalarArray:1.0".into(),
        fields: vec![
            ("value".into(), FieldDesc::ScalarArray(element_type)),
            ("alarm".into(), alarm_desc()),
            ("timeStamp".into(), timestamp_desc()),
            ("display".into(), display_desc()),
        ],
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
    alarm.fields.push((
        "status".into(),
        PvField::Scalar(ScalarValue::Int(snapshot.alarm.status as i32)),
    ));
    alarm.fields.push((
        "message".into(),
        PvField::Scalar(ScalarValue::String(alarm_severity_string(
            snapshot.alarm.severity,
        ))),
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
    alarm.fields.push((
        "status".into(),
        PvField::Scalar(ScalarValue::Int(eff_status as i32)),
    ));
    alarm.fields.push((
        "message".into(),
        PvField::Scalar(ScalarValue::String(alarm_severity_string(eff_severity))),
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

fn build_display(disp: &DisplayInfo) -> PvStructure {
    let mut d = PvStructure::new("display_t");
    d.fields.push((
        "limitLow".into(),
        PvField::Scalar(ScalarValue::Double(disp.lower_disp_limit)),
    ));
    d.fields.push((
        "limitHigh".into(),
        PvField::Scalar(ScalarValue::Double(disp.upper_disp_limit)),
    ));
    d.fields.push((
        "description".into(),
        PvField::Scalar(ScalarValue::String(disp.description.clone())),
    ));
    d.fields.push((
        "units".into(),
        PvField::Scalar(ScalarValue::String(disp.units.clone())),
    ));
    d.fields.push((
        "precision".into(),
        PvField::Scalar(ScalarValue::Int(disp.precision as i32)),
    ));
    d.fields.push((
        "form".into(),
        PvField::Scalar(ScalarValue::Int(disp.form as i32)),
    ));
    d
}

fn build_control(ctrl: &ControlInfo) -> PvStructure {
    let mut c = PvStructure::new("control_t");
    c.fields.push((
        "limitLow".into(),
        PvField::Scalar(ScalarValue::Double(ctrl.lower_ctrl_limit)),
    ));
    c.fields.push((
        "limitHigh".into(),
        PvField::Scalar(ScalarValue::Double(ctrl.upper_ctrl_limit)),
    ));
    c.fields
        .push(("minStep".into(), PvField::Scalar(ScalarValue::Double(0.0))));
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

fn display_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "display_t".into(),
        fields: vec![
            ("limitLow".into(), FieldDesc::Scalar(ScalarType::Double)),
            ("limitHigh".into(), FieldDesc::Scalar(ScalarType::Double)),
            ("description".into(), FieldDesc::Scalar(ScalarType::String)),
            ("units".into(), FieldDesc::Scalar(ScalarType::String)),
            ("precision".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("form".into(), FieldDesc::Scalar(ScalarType::Int)),
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

fn build_value_alarm(disp: &DisplayInfo) -> PvStructure {
    let mut va = PvStructure::new("valueAlarm_t");
    va.fields.push((
        "lowAlarmLimit".into(),
        PvField::Scalar(ScalarValue::Double(disp.lower_alarm_limit)),
    ));
    va.fields.push((
        "lowWarningLimit".into(),
        PvField::Scalar(ScalarValue::Double(disp.lower_warning_limit)),
    ));
    va.fields.push((
        "highWarningLimit".into(),
        PvField::Scalar(ScalarValue::Double(disp.upper_warning_limit)),
    ));
    va.fields.push((
        "highAlarmLimit".into(),
        PvField::Scalar(ScalarValue::Double(disp.upper_alarm_limit)),
    ));
    va
}

fn value_alarm_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "valueAlarm_t".into(),
        fields: vec![
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
        ],
    }
}

fn alarm_severity_string(severity: u16) -> String {
    match severity {
        0 => "NO_ALARM".into(),
        1 => "MINOR".into(),
        2 => "MAJOR".into(),
        3 => "INVALID".into(),
        _ => format!("UNKNOWN({severity})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_base_rs::server::snapshot::{AlarmInfo, EnumInfo, Snapshot};

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
        let snap = test_snapshot(EpicsValue::DoubleArray(vec![]));
        let pv = snapshot_to_nt_scalar(&snap);

        if let Some(PvField::Structure(alarm)) = pv.get_field("alarm") {
            let sev = alarm.get_field("severity");
            let st = alarm.get_field("status");
            assert!(matches!(sev, Some(PvField::Scalar(ScalarValue::Int(3)))));
            assert!(matches!(st, Some(PvField::Scalar(ScalarValue::Int(17)))));
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

    #[test]
    fn nt_enum_structure() {
        let mut snap = Snapshot::new(EpicsValue::Enum(1), 0, 0, UNIX_EPOCH);
        snap.enums = Some(EnumInfo {
            strings: vec!["Off".into(), "On".into()],
        });
        let pv = snapshot_to_nt_enum(&snap);

        assert_eq!(pv.struct_id, "epics:nt/NTEnum:1.0");
        // value is a sub-structure
        if let Some(PvField::Structure(val)) = pv.get_field("value") {
            if let Some(PvField::Scalar(ScalarValue::UShort(idx))) = val.get_field("index") {
                assert_eq!(*idx, 1);
            } else {
                panic!("expected index scalar");
            }
            if let Some(PvField::ScalarArray(choices)) = val.get_field("choices") {
                assert_eq!(choices.len(), 2);
            } else {
                panic!("expected choices array");
            }
        } else {
            panic!("expected value structure");
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
    fn field_desc_nt_enum_index_ushort() {
        let desc = build_nt_enum_desc();
        if let FieldDesc::Structure { fields, .. } = &desc {
            if let Some((
                _,
                FieldDesc::Structure {
                    fields: val_fields, ..
                },
            )) = fields.iter().find(|(n, _)| n == "value")
            {
                let index_field = val_fields.iter().find(|(n, _)| n == "index");
                assert!(matches!(
                    index_field,
                    Some((_, FieldDesc::Scalar(ScalarType::UShort)))
                ));
            } else {
                panic!("expected value structure");
            }
        } else {
            panic!("expected structure");
        }
    }
}
