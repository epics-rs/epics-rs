#![cfg(tokio_backend)]
//! ml_monoCtl reads autoMode and ccMode as C's monitored variables, written
//! as each monitor arrives (ml_monoCtl.st:55-64, no efflag).

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
/// though E is taken before Mode.
#[tokio::test(flavor = "current_thread")]
async fn an_e_put_queued_after_auto_mode_moves() {
    let server = boot([10.0, 10.0, 0.0, 100.0]).await;
    settled(&server, "ml_mono Control Ready").await;
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
        if theta != 10.0 && f64_of(&server, "M:th.VAL").await == theta {
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
