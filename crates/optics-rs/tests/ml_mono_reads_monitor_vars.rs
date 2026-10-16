#![cfg(tokio_backend)]
//! ml_monoCtl reads autoMode and ccMode as C's monitored variables, written
//! as each monitor arrives (ml_monoCtl.st:55-64, no efflag), and its
//! updateSet state set ties UseSet to the motor .SET fields (:1262-1305).

use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::{CaServer, CaServerBuilder};
use optics_rs::snl::ml_mono_ctl::{self, MlMonoConfig};
use std::collections::HashMap;
use std::time::Duration;

const ML_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db/ml_monoSeq.db");
const MOTORS: &str = r#"
record(motor, "M:th") { field(HLM, "90") field(LLM, "-10") }
record(motor, "M:th2") { field(HLM, "90") field(LLM, "-10") }
record(motor, "M:y") { field(HLM, "250") field(LLM, "-250") }
record(motor, "M:z") { field(HLM, "500") field(LLM, "-500") }
"#;

async fn f64_of(server: &CaServer, pv: &str) -> f64 {
    server.get(pv).await.unwrap().to_f64().unwrap()
}

async fn string_of(server: &CaServer, pv: &str) -> String {
    match server.get(pv).await.unwrap() {
        EpicsValue::String(s) => s.to_string(),
        v => format!("{v:?}"),
    }
}

async fn boot(rbv: [f64; 4]) -> CaServer {
    let macros: HashMap<String, String> = [("P", "M:")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_record_type("motor", || Box::new(motor_rs::MotorRecord::new()))
        .db_string(MOTORS, &macros)
        .unwrap()
        .db_file(ML_DB, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = (**server.database()).clone();
    for (name, rbv) in ["M:th", "M:th2", "M:y", "M:z"].into_iter().zip(rbv) {
        db.get_record(name)
            .unwrap()
            .write()
            .record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<motor_rs::MotorRecord>())
            .unwrap()
            .pos
            .rbv = rbv;
    }
    tokio::spawn(ml_mono_ctl::run(
        MlMonoConfig::new("M:", "th", "th2", "y", "z", 35.0, 0),
        db,
    ));
    server
}

/// Poll until SeqMsg1 reads `msg1`.
async fn settled(server: &CaServer, msg1: &str) {
    for _ in 0..200 {
        if string_of(server, "M:ml_monoSeqMsg1").await == msg1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("SeqMsg1 never read {msg1:?}");
}

/// An E put queued with Mode=Auto already sees auto mode and drives theta,
/// though E is taken before Mode. The boot's own yOffset put wakes the
/// yOffset arm last (C kohzuIoc oracle, y at 35: "y offset changed to
/// 35.000000", then E x1.01 drives th to 9.89998).
#[tokio::test(flavor = "current_thread")]
async fn an_e_put_queued_after_auto_mode_moves() {
    let server = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&server, "y offset changed to 35.000000").await;
    // Past the waitForCmndEnter that clears E 0.1 s after that pass.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let db = (**server.database()).clone();
    let e = f64_of(&server, "M:ml_monoE").await;
    // No await between the posts: on this one-thread runtime the sequencer
    // finds both queued.
    for (pv, v) in [
        ("M:ml_monoMode", EpicsValue::Enum(1)),
        ("M:ml_monoE", EpicsValue::Double(e * 1.01)),
    ] {
        let rec = db.get_record(pv).unwrap();
        let mut inst = rec.write();
        inst.record.put_field("VAL", v).unwrap();
        inst.notify_field("VAL", EventMask::VALUE | EventMask::LOG);
    }
    for _ in 0..100 {
        let theta = f64_of(&server, "M:ml_monoTheta").await;
        if (theta - 9.89998).abs() < 1e-5 && f64_of(&server, "M:th.VAL").await == theta {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "theta {} not driven: th.VAL {}",
        f64_of(&server, "M:ml_monoTheta").await,
        f64_of(&server, "M:th.VAL").await
    );
}

/// Write `pv` and post it as a C client's put would, with no await between.
fn post(server: &CaServer, pv: &str, v: EpicsValue) {
    let db = (**server.database()).clone();
    let rec = db.get_record(pv).unwrap();
    let mut inst = rec.write();
    inst.record.put_field("VAL", v).unwrap();
    inst.notify_field("VAL", EventMask::VALUE | EventMask::LOG);
}

async fn theta_driven_to(server: &CaServer, want: f64) {
    for _ in 0..100 {
        let theta = f64_of(server, "M:ml_monoTheta").await;
        if (theta - want).abs() < 1e-5 && f64_of(server, "M:th.VAL").await == theta {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "theta {} not driven: th.VAL {}",
        f64_of(server, "M:ml_monoTheta").await,
        f64_of(server, "M:th.VAL").await
    );
}

/// OperAck 0 makes no waitForCmnd clause true (ml_monoCtl.st:705), so the
/// state set stays put and the E put that follows drives theta as in C.
#[tokio::test(flavor = "current_thread")]
async fn an_oper_ack_0_leaves_the_e_put_after_it() {
    let server = boot([10.0, 10.0, 35.0, 100.0]).await;
    settled(&server, "y offset changed to 35.000000").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let e = f64_of(&server, "M:ml_monoE").await;
    post(&server, "M:ml_monoMode", EpicsValue::Enum(1));
    post(&server, "M:ml_monoOperAck", EpicsValue::Enum(0));
    tokio::time::sleep(Duration::from_millis(30)).await;
    post(&server, "M:ml_monoE", EpicsValue::Double(e * 1.01));
    theta_driven_to(&server, 9.89998).await;
}

/// C kohzuIoc oracle, th at 0: chkMotorLimits holds the init sequence,
/// "Theta Readback of 0 not reasonable" every 2 s until the readback moves; then "All motors
/// within limits" and the sequence runs on to its yOffset pass.
#[tokio::test(flavor = "current_thread")]
async fn a_theta_readback_of_0_holds_init_in_chk_motor_limits() {
    let server = boot([0.0, 10.0, 35.0, 100.0]).await;
    settled(&server, "Theta Readback of 0 not reasonable").await;
    // Two chkMotorLimits rounds: msg2 reads "Modify Motor Positions/Limits"
    // only until the next round puts "Set to Manual Mode" right after it.
    tokio::time::sleep(Duration::from_millis(4500)).await;
    assert_eq!(
        string_of(&server, "M:ml_monoSeqMsg1").await,
        "Theta Readback of 0 not reasonable"
    );
    assert_eq!(f64_of(&server, "M:ml_monoAlert").await, 1.0);

    let db = (**server.database()).clone();
    {
        let rec = db.get_record("M:th").unwrap();
        let mut inst = rec.write();
        inst.record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<motor_rs::MotorRecord>())
            .unwrap()
            .pos
            .rbv = 10.0;
        inst.notify_field("RBV", EventMask::VALUE | EventMask::LOG);
    }
    settled(&server, "y offset changed to 35.000000").await;
}

async fn set_flags(server: &CaServer) -> [f64; 4] {
    let mut flags = [0.0; 4];
    for (flag, pv) in flags
        .iter_mut()
        .zip(["M:th.SET", "M:th2.SET", "M:z.SET", "M:y.SET"])
    {
        *flag = f64_of(server, pv).await;
    }
    flags
}

async fn put_process(server: &CaServer, pv: &str, v: f64) {
    optics_rs::db_access::DbChannel::new(server.database(), pv)
        .put_f64_process(v)
        .await
        .unwrap();
}

/// C kohzuIoc oracle: UseSet 1 sets th/th2/z .SET and leaves y alone; a
/// th2.SET 0 then takes UseSet, and through it th and z, back to Use.
#[tokio::test]
async fn use_set_and_the_motor_set_fields_follow_each_other() {
    let server = boot([10.0, 10.0, 0.0, 100.0]).await;
    put_process(&server, "M:ml_monoUseSet", 1.0).await;
    let mut flags = [0.0; 4];
    for _ in 0..100 {
        flags = set_flags(&server).await;
        if flags == [1.0, 1.0, 1.0, 0.0] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        flags,
        [1.0, 1.0, 1.0, 0.0],
        "th, th2, z, y .SET after UseSet 1"
    );

    put_process(&server, "M:th2.SET", 0.0).await;
    for _ in 0..100 {
        flags = set_flags(&server).await;
        if f64_of(&server, "M:ml_monoUseSet").await == 0.0 && flags == [0.0; 4] {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "UseSet {} .SET {flags:?} after th2.SET 0",
        f64_of(&server, "M:ml_monoUseSet").await
    );
}
