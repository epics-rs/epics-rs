//! Smoke test for `#[derive(NTScalar)]` + `pvget_typed` /
//! `pvput_typed` / `pvmonitor_typed`. Spins up an in-process
//! `PvaServer` with a single SharedPV, then exercises every
//! typed-NT entry point of `PvaClient`.

use std::sync::Arc;
use std::time::Duration;

use epics_pva_rs::nt::derive::{NTScalar, NTTable};
use epics_pva_rs::nt::typed::EnumValue;
use epics_pva_rs::nt::{Alarm, TimeStamp, TypedNT};
use epics_pva_rs::pvdata::{FieldDesc, ScalarType};
use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};
// PVA listener tests run in parallel: PvaServer::start now binds
// the TCP listener synchronously inside `start()` so the
// pick-and-drop race that motivated file_serial is gone. CA-side
// softIoc tests still need cross-binary serialisation because the
// C process owns the EPICS_CA_SERVER_PORT env var globally.

#[derive(Debug, Clone, NTScalar, PartialEq)]
struct MotorPos {
    value: f64,
    #[nt(meta)]
    alarm: Alarm,
    #[nt(meta)]
    timestamp: TimeStamp,
}

#[test]
fn typed_nt_descriptor_shape() {
    let d = MotorPos::descriptor();
    match d {
        FieldDesc::Structure { struct_id, fields } => {
            assert_eq!(struct_id, "epics:nt/NTScalar:1.0");
            // value + alarm + timestamp
            assert_eq!(fields.len(), 3);
            assert_eq!(fields[0].0, "value");
            assert!(matches!(fields[0].1, FieldDesc::Scalar(ScalarType::Double)));
            assert_eq!(fields[1].0, "alarm");
            assert_eq!(fields[2].0, "timestamp");
        }
        other => panic!("unexpected descriptor: {other:?}"),
    }
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
        timestamp: TimeStamp {
            seconds_past_epoch: 1_700_000_000,
            nanoseconds: 12345,
            user_tag: 7,
        },
    };
    let f = pos.to_pv_field();
    let back = MotorPos::from_pv_field(&f).expect("decode");
    assert_eq!(pos, back);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pvget_typed_against_local_server() {
    // Build a SharedPV holding a 3-field NTScalar (value + alarm +
    // timestamp). The descriptor we open with must match the
    // derived MotorPos descriptor exactly.
    let pv = SharedPV::new();
    pv.open(MotorPos::descriptor(), {
        let initial = MotorPos {
            value: 42.5,
            alarm: Alarm {
                severity: 0,
                status: 0,
                message: String::new(),
            },
            timestamp: TimeStamp::default(),
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

/// I-2: same NTScalar derive, value field is `Vec<f64>` — wrapper
/// struct_id auto-flips to `epics:nt/NTScalarArray:1.0`.
#[derive(Debug, Clone, NTScalar, PartialEq)]
struct Trajectory {
    value: Vec<f64>,
    #[nt(meta)]
    alarm: Alarm,
}

#[test]
fn typed_nt_array_descriptor() {
    let d = Trajectory::descriptor();
    match d {
        FieldDesc::Structure { struct_id, fields } => {
            assert_eq!(struct_id, "epics:nt/NTScalarArray:1.0");
            assert_eq!(fields[0].0, "value");
            assert!(matches!(
                fields[0].1,
                FieldDesc::ScalarArray(ScalarType::Double)
            ));
        }
        other => panic!("unexpected descriptor: {other:?}"),
    }
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

/// I-2: NTEnum via EnumValue runtime helper.
#[derive(Debug, Clone, NTScalar, PartialEq)]
struct ValveState {
    value: EnumValue,
    #[nt(meta)]
    alarm: Alarm,
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

    // Wrapper struct_id is NTEnum (forwarded from EnumValue::descriptor()).
    let d = ValveState::descriptor();
    match d {
        FieldDesc::Structure { struct_id, .. } => {
            assert_eq!(struct_id, "epics:nt/NTEnum:1.0");
        }
        other => panic!("unexpected descriptor: {other:?}"),
    }
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

/// I-2 follow-up: NTTable derive — multi-column table.
#[derive(Debug, Clone, NTTable, PartialEq)]
struct ScanResult {
    timestamp: Vec<f64>,
    position: Vec<f64>,
    intensity: Vec<f64>,
}

#[test]
fn typed_nt_table_descriptor_shape() {
    let d = ScanResult::descriptor();
    match d {
        FieldDesc::Structure { struct_id, fields } => {
            assert_eq!(struct_id, "epics:nt/NTTable:1.0");
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].0, "labels");
            assert!(matches!(
                fields[0].1,
                FieldDesc::ScalarArray(ScalarType::String)
            ));
            assert_eq!(fields[1].0, "value");
            match &fields[1].1 {
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
        other => panic!("unexpected NTTable descriptor: {other:?}"),
    }
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
