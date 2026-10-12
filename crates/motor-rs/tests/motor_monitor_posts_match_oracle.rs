//! motor's `monitor()` MARK posts (`motorRecord.cc:3456-3646`) against
//! oracleMotorIoc (motorRecord + the asyn motorSim axis, MRES 0.001, VELO 1,
//! DHLM 100, DLLM -100), three `camonitor -m v|a|l` clients on `M1 M1.RBV
//! M1.OFF M1.HLM M1.LLM M1.DHLM M1.DLLM M1.DIR M1.DMOV M1.LVIO` started after
//! a `caput M1 1` move. The streams below are the oracle's with the timestamps
//! and the connect values cut, compared per PV: the order across fields within
//! one cycle is the framework's.

mod common;

use common::{MotorFixture, drain};
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const PVS: [(&str, &str, DbFieldType); 10] = [
    ("M1", "VAL", DbFieldType::Double),
    ("M1.RBV", "RBV", DbFieldType::Double),
    ("M1.OFF", "OFF", DbFieldType::Double),
    ("M1.HLM", "HLM", DbFieldType::Double),
    ("M1.LLM", "LLM", DbFieldType::Double),
    ("M1.DHLM", "DHLM", DbFieldType::Double),
    ("M1.DLLM", "DLLM", DbFieldType::Double),
    ("M1.DIR", "DIR", DbFieldType::Enum),
    ("M1.DMOV", "DMOV", DbFieldType::Short),
    ("M1.LVIO", "LVIO", DbFieldType::Short),
];

/// `caput M1.HLM 50; M1.LLM -20; M1.DIR 1; M1.HLM 60; M1.OFF 2`: a user-limit
/// put posts its field from `dbPut` and again from its MARK, then the dial
/// limit it moved; DIR and OFF post RBV, OFF or VAL and both user limits from
/// their marks whether or not the value moved.
const LIMITS: &str = "\
M1.HLM 50
M1.HLM 50
M1.DHLM 50
M1.LLM -20
M1.LLM -20
M1.DLLM -20
M1.DIR Neg
M1.RBV 1
M1.HLM 22
M1.LLM -48
M1.OFF 2
M1.HLM 60
M1.HLM 60
M1.DLLM -58
M1.OFF 2
M1.RBV 1
M1 1
M1.HLM 60
M1.LLM -48
";

/// `caput M1.SET 1; M1.VAL 5; M1.SET 0` on a fresh record: the drive put's
/// `special()` posts the DMOV blink, and the cycle's offset redefinition
/// (do_work 2206-2227) marks RBV, VAL, OFF, both user limits and DMOV back.
const SET_MODE: &str = "\
M1.DMOV 0
M1.RBV 5
M1 5
M1.HLM 105
M1.LLM -95
M1.OFF 5
M1.DMOV 1
";

/// Each class's reader per PV, in `PVS` order.
fn subscribe(fx: &MotorFixture) -> Vec<Vec<EventReader>> {
    let mut sid = 0;
    [EventMask::VALUE, EventMask::ALARM, EventMask::LOG]
        .into_iter()
        .map(|class| {
            PVS.iter()
                .map(|&(_, field, ty)| {
                    sid += 1;
                    fx.subscribe(field, sid, ty, class)
                })
                .collect()
        })
        .collect()
}

/// What one class's readers carried, as `camonitor` prints it per PV.
fn lines(readers: &mut [EventReader]) -> Vec<String> {
    PVS.iter()
        .zip(readers)
        .flat_map(|(&(pv, _, _), rx)| {
            drain(rx).into_iter().map(move |v| {
                let v = match v {
                    EpicsValue::Double(v) => format!("{v}"),
                    EpicsValue::Short(v) => format!("{v}"),
                    EpicsValue::Enum(v) => ["Pos", "Neg"][usize::from(v)].to_string(),
                    other => panic!("{pv}: {other:?}"),
                };
                format!("{pv} {v}")
            })
        })
        .collect()
}

/// The oracle's stream in the same per-PV order.
fn expected(stream: &str) -> Vec<String> {
    PVS.iter()
        .flat_map(|&(pv, _, _)| {
            stream
                .lines()
                .filter(move |l| l.rsplit_once(' ').unwrap().0 == pv)
                .map(str::to_string)
        })
        .collect()
}

fn assert_streams(readers: &mut [Vec<EventReader>], stream: &str) {
    assert_eq!(lines(&mut readers[0]), expected(stream), "DBE_VALUE");
    assert_eq!(lines(&mut readers[1]), Vec::<String>::new(), "DBE_ALARM");
    assert_eq!(lines(&mut readers[2]), expected(stream), "DBE_LOG");
}

async fn put(fx: &MotorFixture, field: &str, value: f64) {
    fx.db
        .put_record_field_from_ca("M1", field, EpicsValue::Double(value))
        .await
        .unwrap();
}

#[tokio::test]
async fn limit_dir_and_off_puts_post_their_marks_as_the_oracle_does() {
    let mut fx = MotorFixture::new(|rec| rec.vel.velo = 1.0).await;
    fx.move_to(1.0).await;
    let mut readers = subscribe(&fx);
    for (field, value) in [
        ("HLM", 50.0),
        ("LLM", -20.0),
        ("DIR", 1.0),
        ("HLM", 60.0),
        ("OFF", 2.0),
    ] {
        put(&fx, field, value).await;
    }
    assert_streams(&mut readers, LIMITS);
    // The oracle's `caget` after the steps.
    for (field, v) in [
        ("VAL", 1.0),
        ("OFF", 2.0),
        ("HLM", 60.0),
        ("LLM", -48.0),
        ("DHLM", 50.0),
        ("DLLM", -58.0),
        ("RBV", 1.0),
    ] {
        let pv = format!("M1.{field}");
        assert_eq!(fx.db.get_pv(&pv).unwrap(), EpicsValue::Double(v), "{pv}");
    }
}

#[tokio::test]
async fn a_set_mode_val_put_posts_the_blink_and_the_redefinition_as_the_oracle_does() {
    let fx = MotorFixture::new(|rec| rec.vel.velo = 1.0).await;
    let mut readers = subscribe(&fx);
    for (field, value) in [("SET", 1.0), ("VAL", 5.0), ("SET", 0.0)] {
        put(&fx, field, value).await;
    }
    assert_streams(&mut readers, SET_MODE);
    // The oracle's `caget` after the steps.
    for (field, v) in [
        ("VAL", 5.0),
        ("OFF", 5.0),
        ("HLM", 105.0),
        ("LLM", -95.0),
        ("RBV", 5.0),
    ] {
        let pv = format!("M1.{field}");
        assert_eq!(fx.db.get_pv(&pv).unwrap(), EpicsValue::Double(v), "{pv}");
    }
}

/// The same steps behind `caput M1.SPMG 0`: the oracle redefines under Stop
/// too, since C's VAL collection precedes the stop_or_pause return (2237).
#[tokio::test]
async fn a_set_mode_val_put_under_spmg_stop_redefines_as_the_oracle_does() {
    let fx = MotorFixture::new(|rec| rec.vel.velo = 1.0).await;
    let mut readers = subscribe(&fx);
    for (field, value) in [("SPMG", 0.0), ("SET", 1.0), ("VAL", 5.0), ("SET", 0.0)] {
        put(&fx, field, value).await;
    }
    assert_streams(&mut readers, SET_MODE);
}
