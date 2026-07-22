//! Smoke test for `#[derive(NTScalar)]` + `pvget_typed` /
//! `pvput_typed` / `pvmonitor_typed`. Spins up an in-process
//! `PvaServer` with a single SharedPV, then exercises every
//! typed-NT entry point of `PvaClient`.
//!
//! The descriptor tests are builder-parity checks: a `#[derive]`d
//! Normative Type must carry the mandatory metadata members for the
//! structure ID it claims, matching the runtime `NTScalar` / `NTTable`
//! builders (which mirror pvxs `nt.cpp`). A derive that emits only
//! `value` (+ one user meta) under a normative ID is the bug those
//! tests guard against.

// RTEMS-EXEC-MODEL-ALLOW(2): checked - these run and pass in the feature-ON suite.

use std::sync::Arc;
use std::time::Duration;

use epics_pva_rs::nt::derive::{NTScalar, NTTable};
use epics_pva_rs::nt::typed::EnumValue;
use epics_pva_rs::nt::{Alarm, NTScalar as NTScalarBuilder, TypedNT, meta};
use epics_pva_rs::pvdata::{FieldDesc, ScalarType};
use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};
// PVA listener tests run in parallel: PvaServer::start now binds
// the TCP listener synchronously inside `start()` so the
// pick-and-drop race that motivated file_serial is gone. CA-side
// softIoc tests still need cross-binary serialisation because the
// C process owns the EPICS_CA_SERVER_PORT env var globally.

/// Top-level member names of a structure descriptor, in order.
fn member_names(d: &FieldDesc) -> Vec<String> {
    match d {
        FieldDesc::Structure { fields, .. } => fields.iter().map(|(n, _)| n.clone()).collect(),
        other => panic!("expected structure descriptor, got {other:?}"),
    }
}

/// The descriptor of one named member.
fn member<'a>(d: &'a FieldDesc, name: &str) -> &'a FieldDesc {
    match d {
        FieldDesc::Structure { fields, .. } => {
            &fields
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("no member named {name:?}"))
                .1
        }
        other => panic!("expected structure descriptor, got {other:?}"),
    }
}

/// User declares the alarm meta explicitly; `timeStamp` must be filled
/// in by the derive even though it is not a struct field, so the wrapper
/// matches the normative NTScalar layout (value + alarm + timeStamp).
#[derive(Debug, Clone, NTScalar, PartialEq)]
struct MotorPos {
    value: f64,
    #[nt(meta)]
    alarm: Alarm,
}

#[test]
fn typed_nt_descriptor_is_full_ntscalar() {
    let d = MotorPos::descriptor();
    match &d {
        FieldDesc::Structure { struct_id, .. } => {
            assert_eq!(struct_id, "epics:nt/NTScalar:1.0");
        }
        other => panic!("unexpected descriptor: {other:?}"),
    }
    // Mandatory members present, in canonical pvxs order.
    assert_eq!(member_names(&d), vec!["value", "alarm", "timeStamp"]);
    assert!(matches!(
        member(&d, "value"),
        FieldDesc::Scalar(ScalarType::Double)
    ));
    // Builder parity: the metadata members equal what the runtime
    // NTScalar builder emits for a double (which mirrors pvxs nt.cpp).
    let rt = NTScalarBuilder::new(ScalarType::Double).build();
    assert_eq!(member(&d, "alarm"), member(&rt, "alarm"));
    assert_eq!(member(&d, "timeStamp"), member(&rt, "timeStamp"));
    assert_eq!(member(&d, "alarm"), &meta::alarm_desc());
    assert_eq!(member(&d, "timeStamp"), &meta::time_desc());
}

#[test]
fn typed_nt_value_carries_mandatory_metadata() {
    // The value path must mirror the descriptor: alarm + timeStamp
    // present even though the user only set `value` + `alarm`.
    let pos = MotorPos {
        value: 2.71,
        alarm: Alarm {
            severity: 1,
            status: 2,
            message: "near limit".into(),
        },
    };
    let f = pos.to_pv_field();
    let epics_pva_rs::pvdata::PvField::Structure(s) = &f else {
        panic!("expected structure value, got {f:?}");
    };
    assert_eq!(s.struct_id, "epics:nt/NTScalar:1.0");
    let names: Vec<&str> = s.fields.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["value", "alarm", "timeStamp"]);
    // timeStamp defaulted (epoch zero) since the user did not set it.
    assert_eq!(s.get_field("timeStamp"), Some(&meta::time_default()));
}

#[test]
fn typed_nt_round_trip_local() {
    let pos = MotorPos {
        value: 2.71,
        alarm: Alarm {
            severity: 1,
            status: 2,
            message: "near limit".into(),
        },
    };
    let f = pos.to_pv_field();
    let back = MotorPos::from_pv_field(&f).expect("decode");
    assert_eq!(pos, back);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pvget_typed_against_local_server() {
    // Build a SharedPV holding the derived NTScalar. The descriptor we
    // open with must match the derived MotorPos descriptor exactly.
    let pv = SharedPV::new();
    pv.open(MotorPos::descriptor(), {
        let initial = MotorPos {
            value: 42.5,
            alarm: Alarm {
                severity: 0,
                status: 0,
                message: String::new(),
            },
        };
        initial.to_pv_field()
    })
    .unwrap();
    let source = SharedSource::new();
    source.add("MOTOR:VAL", pv);
    let _server = PvaServer::isolated(Arc::new(source)).expect("isolated test server must start");
    let client = _server.client_config();
    let _ = &_server;

    let pos: MotorPos = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvget_typed::<MotorPos>("MOTOR:VAL"),
    )
    .await
    .expect("timeout")
    .expect("typed get");
    assert_eq!(pos.value, 42.5);
    assert_eq!(pos.alarm.severity, 0);
    assert_eq!(pos.alarm.message, "");
}

/// Same NTScalar derive, value field is `Vec<f64>` — wrapper struct_id
/// auto-flips to `epics:nt/NTScalarArray:1.0`. The user declares only
/// `alarm`; `timeStamp` must still be filled in.
#[derive(Debug, Clone, NTScalar, PartialEq)]
struct Trajectory {
    value: Vec<f64>,
    #[nt(meta)]
    alarm: Alarm,
}

#[test]
fn typed_nt_array_descriptor_is_full_ntscalararray() {
    let d = Trajectory::descriptor();
    match &d {
        FieldDesc::Structure { struct_id, .. } => {
            assert_eq!(struct_id, "epics:nt/NTScalarArray:1.0");
        }
        other => panic!("unexpected descriptor: {other:?}"),
    }
    // The mandatory timeStamp the original truncated derive omitted is
    // now present, in canonical order.
    assert_eq!(member_names(&d), vec!["value", "alarm", "timeStamp"]);
    assert!(matches!(
        member(&d, "value"),
        FieldDesc::ScalarArray(ScalarType::Double)
    ));
    let rt = NTScalarBuilder::array(ScalarType::Double).build();
    assert_eq!(member(&d, "alarm"), member(&rt, "alarm"));
    assert_eq!(member(&d, "timeStamp"), member(&rt, "timeStamp"));
}

#[test]
fn typed_nt_array_round_trip() {
    let t = Trajectory {
        value: vec![1.0, 2.0, 3.0, 4.0],
        alarm: Alarm::default(),
    };
    let f = t.to_pv_field();
    let back = Trajectory::from_pv_field(&f).expect("decode");
    assert_eq!(t, back);
}

/// NTEnum via the EnumValue runtime helper. The user declares only
/// `alarm`; the derive must add `timeStamp` AND the NTEnum `display`
/// baseline (pvxs nt.cpp:121-131).
#[derive(Debug, Clone, NTScalar, PartialEq)]
struct ValveState {
    value: EnumValue,
    #[nt(meta)]
    alarm: Alarm,
}

#[test]
fn typed_nt_enum_descriptor_has_full_ntenum_baseline() {
    let d = ValveState::descriptor();
    match &d {
        FieldDesc::Structure { struct_id, .. } => {
            assert_eq!(struct_id, "epics:nt/NTEnum:1.0");
        }
        other => panic!("unexpected descriptor: {other:?}"),
    }
    // pvxs NTEnum::build: value, alarm, timeStamp, display{description}.
    assert_eq!(
        member_names(&d),
        vec!["value", "alarm", "timeStamp", "display"]
    );
    assert_eq!(member(&d, "alarm"), &meta::alarm_desc());
    assert_eq!(member(&d, "timeStamp"), &meta::time_desc());
    // display is a bare struct with exactly `description: String`.
    match member(&d, "display") {
        FieldDesc::Structure { struct_id, fields } => {
            assert_eq!(struct_id, "");
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].0, "description");
            assert!(matches!(fields[0].1, FieldDesc::Scalar(ScalarType::String)));
        }
        other => panic!("unexpected display descriptor: {other:?}"),
    }
}

#[test]
fn typed_nt_enum_round_trip() {
    let v = ValveState {
        value: EnumValue {
            index: 1,
            choices: vec!["closed".into(), "open".into(), "fault".into()],
        },
        alarm: Alarm::default(),
    };
    let f = v.to_pv_field();
    let back = ValveState::from_pv_field(&f).expect("decode");
    assert_eq!(v, back);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pvget_typed_primitive_f64() {
    // Bare f64 against a plain NTScalar<double> source.
    let pv = SharedPV::new();
    pv.open(f64::descriptor(), f64::to_pv_field(&7.5)).unwrap();
    let source = SharedSource::new();
    source.add("OVEN:TEMP", pv);
    let _server = PvaServer::isolated(Arc::new(source)).expect("isolated test server must start");
    let client = _server.client_config();

    let temp: f64 = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvget_typed::<f64>("OVEN:TEMP"),
    )
    .await
    .expect("timeout")
    .expect("typed get");
    assert_eq!(temp, 7.5);
}

/// NTTable derive — multi-column table. The derive must add the
/// normative `descriptor`, `alarm`, and `timeStamp` members pvxs
/// `NTTable::build()` always emits (nt.cpp:170-176).
#[derive(Debug, Clone, NTTable, PartialEq)]
struct ScanResult {
    timestamp: Vec<f64>,
    position: Vec<f64>,
    intensity: Vec<f64>,
}

#[test]
fn typed_nt_table_descriptor_is_full_nttable() {
    let d = ScanResult::descriptor();
    match &d {
        FieldDesc::Structure { struct_id, .. } => {
            assert_eq!(struct_id, "epics:nt/NTTable:1.0");
        }
        other => panic!("unexpected NTTable descriptor: {other:?}"),
    }
    // pvxs order: labels, value, descriptor, alarm, timeStamp.
    assert_eq!(
        member_names(&d),
        vec!["labels", "value", "descriptor", "alarm", "timeStamp"]
    );
    assert!(matches!(
        member(&d, "labels"),
        FieldDesc::ScalarArray(ScalarType::String)
    ));
    assert!(matches!(
        member(&d, "descriptor"),
        FieldDesc::Scalar(ScalarType::String)
    ));
    assert_eq!(member(&d, "alarm"), &meta::alarm_desc());
    assert_eq!(member(&d, "timeStamp"), &meta::time_desc());
    // The column sub-structure is unchanged.
    match member(&d, "value") {
        FieldDesc::Structure { fields: cols, .. } => {
            assert_eq!(cols.len(), 3);
            assert_eq!(cols[0].0, "timestamp");
            assert_eq!(cols[1].0, "position");
            assert_eq!(cols[2].0, "intensity");
            for (_, col_desc) in cols {
                assert!(matches!(
                    col_desc,
                    FieldDesc::ScalarArray(ScalarType::Double)
                ));
            }
        }
        other => panic!("unexpected value descriptor: {other:?}"),
    }
}

#[test]
fn typed_nt_table_value_carries_mandatory_metadata() {
    let scan = ScanResult {
        timestamp: vec![1.0, 2.0, 3.0],
        position: vec![10.0, 20.0, 30.0],
        intensity: vec![0.1, 0.2, 0.3],
    };
    let f = scan.to_pv_field();
    let epics_pva_rs::pvdata::PvField::Structure(s) = &f else {
        panic!("expected structure value, got {f:?}");
    };
    let names: Vec<&str> = s.fields.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec!["labels", "value", "descriptor", "alarm", "timeStamp"]
    );
    assert_eq!(s.get_field("alarm"), Some(&meta::alarm_default()));
    assert_eq!(s.get_field("timeStamp"), Some(&meta::time_default()));
}

#[test]
fn typed_nt_table_round_trip() {
    let scan = ScanResult {
        timestamp: vec![1.0, 2.0, 3.0],
        position: vec![10.0, 20.0, 30.0],
        intensity: vec![0.1, 0.2, 0.3],
    };
    let f = scan.to_pv_field();
    let back = ScanResult::from_pv_field(&f).expect("decode");
    assert_eq!(scan, back);
}
