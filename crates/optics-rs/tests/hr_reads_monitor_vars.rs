#![cfg(tokio_backend)]
//! hrCtl reads autoMode as C's monitored variable, written as the monitor
//! arrives (hrCtl.st:64-66, no efflag).

use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::{CaServer, CaServerBuilder};
use optics_rs::snl::hr_ctl::{self, HrConfig};
use std::collections::HashMap;
use std::time::Duration;

const HR_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db/hrSeq.db");
const MOTORS: &str = r#"
record(motor, "H:p1") { field(HLM, "90") field(LLM, "-10") }
record(motor, "H:p2") { field(HLM, "90") field(LLM, "-10") }
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

async fn boot() -> CaServer {
    let macros: HashMap<String, String> =
        [("P", "H:"), ("N", "1"), ("M_PHI1", "p1"), ("M_PHI2", "p2")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_record_type("motor", || Box::new(motor_rs::MotorRecord::new()))
        .db_string(MOTORS, &macros)
        .unwrap()
        .db_file(HR_DB, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = (**server.database()).clone();
    for name in ["H:p1", "H:p2"] {
        db.get_record(name)
            .unwrap()
            .write()
            .record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<motor_rs::MotorRecord>())
            .unwrap()
            .pos
            .rbv = 10.0;
    }
    tokio::spawn(hr_ctl::run(HrConfig::new("H:", "1", "p1", "p2"), db));
    server
}

/// An E put queued with Mode=Auto already sees auto mode and drives phi1,
/// though E is taken before Mode.
#[tokio::test(flavor = "current_thread")]
async fn an_e_put_queued_after_auto_mode_moves() {
    let server = boot().await;
    for _ in 0..200 {
        if string_of(&server, "H:HR1_SeqMsg1SI").await == "HR Control Ready" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let db = (**server.database()).clone();
    let e = f64_of(&server, "H:HR1_EAO").await;
    let before = f64_of(&server, "H:p1.VAL").await;
    // No await between the posts: on this one-thread runtime the sequencer
    // finds both queued.
    for (pv, v) in [
        ("H:HR1_ModeBO", EpicsValue::Enum(1)),
        ("H:HR1_EAO", EpicsValue::Double(e * 1.01)),
    ] {
        let rec = db.get_record(pv).unwrap();
        let mut inst = rec.write();
        inst.record.put_field("VAL", v).unwrap();
        inst.notify_field("VAL", EventMask::VALUE | EventMask::LOG);
    }
    for _ in 0..100 {
        let set = f64_of(&server, "H:HR1_phi1SetAO").await;
        if set != before && f64_of(&server, "H:p1.VAL").await == set {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "phi1 not driven: phi1SetAO {} p1.VAL {} (was {before})",
        f64_of(&server, "H:HR1_phi1SetAO").await,
        f64_of(&server, "H:p1.VAL").await
    );
}
