#![cfg(tokio_backend)]
//! orient reads motPut_Auto and motGet_Auto as C's monitored variables,
//! written as each monitor arrives (orient_st.st:126, :136, no efflag).

use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::{CaServer, CaServerBuilder};
use optics_rs::snl::orient::{self, OrientConfig};
use std::collections::HashMap;
use std::time::Duration;

const ORIENT_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db/orient.db");
const MOTORS: &str = r#"
record(motor, "X:m:tth") { field(HLM, "180") field(LLM, "-180") }
record(motor, "X:m:th") { field(HLM, "180") field(LLM, "-180") }
record(motor, "X:m:chi") { field(HLM, "180") field(LLM, "-180") }
record(motor, "X:m:phi") { field(HLM, "180") field(LLM, "-180") }
"#;
const P: &str = "X:orient1:";

async fn f64_of(server: &CaServer, pv: &str) -> f64 {
    server.get(pv).await.unwrap().to_f64().unwrap()
}

async fn string_of(server: &CaServer, pv: &str) -> String {
    match server.get(pv).await.unwrap() {
        EpicsValue::String(s) => s.to_string(),
        v => format!("{v:?}"),
    }
}

/// A motor post queued behind motGet_Auto=1 already sees auto mode and
/// copies the motor into the trial angle, though the motor is taken first.
#[tokio::test(flavor = "current_thread")]
async fn a_motor_post_queued_after_mot_get_auto_copies_the_angle() {
    let macros: HashMap<String, String> = [("P", "X:"), ("O", "1"), ("PREC", "4")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .register_record_type("motor", || Box::new(motor_rs::MotorRecord::new()))
        .db_string(MOTORS, &macros)
        .unwrap()
        .db_file(ORIENT_DB, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = (**server.database()).clone();
    tokio::spawn(orient::run(
        OrientConfig::new(P, "X:m:", "tth", "th", "chi", "phi"),
        db.clone(),
    ));
    for _ in 0..200 {
        if string_of(&server, &format!("{P}Msg")).await == "Orient initialized" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // No await between the posts: on this one-thread runtime the sequencer
    // finds both queued.
    for (pv, v) in [
        (format!("{P}motGet_Auto"), EpicsValue::Enum(1)),
        ("X:m:tth".to_string(), EpicsValue::Double(12.5)),
    ] {
        let rec = db.get_record(&pv).unwrap();
        let mut inst = rec.write();
        inst.record.put_field("VAL", v).unwrap();
        inst.notify_field("VAL", EventMask::VALUE | EventMask::LOG);
    }
    for _ in 0..100 {
        if f64_of(&server, &format!("{P}TTH")).await == 12.5 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "TTH {} not copied",
        f64_of(&server, &format!("{P}TTH")).await
    );
}
