//! A CP/CPP edge belongs to the link that makes it. A put of the link field
//! after iocRun is C `dbPutFieldLink`: the old `caLink` goes, a new one
//! connects and processes the holder; a repeated registration replaces the
//! edge rather than adding a second. `VAL+1` counts the processes; every
//! number is softIoc's `dbgf` on the same database (R7.0.10-146).

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, PvString};

const DB: &str = r#"
record(ao, "X") { field(VAL, "0") }
record(calc, "H") { field(CALC, "VAL+1") }
record(calc, "HP") { field(INPA, "X CP") field(CALC, "VAL+1") }
record(calc, "HEV") { field(SCAN, "Event") field(EVNT, "77") field(CALC, "VAL+1") }
"#;

enum Step {
    Link(&'static str, &'static str),
    X(f64),
    Expect(&'static str, f64),
}
use Step::*;

/// softIoc, in order.
const C_SEQUENCE: &[Step] = &[
    Expect("H", 0.0),
    Expect("HP", 1.0),
    Link("H", "X CP"),
    Expect("H", 1.0),
    X(1.0),
    Expect("H", 2.0),
    Expect("HP", 2.0),
    Link("H", "X"),
    Expect("H", 2.0),
    X(2.0),
    Expect("H", 2.0),
    Expect("HP", 3.0),
    Link("HP", "X CPP"),
    Expect("HP", 4.0),
    Link("HEV", "X CPP"),
    Expect("HEV", 0.0),
    X(3.0),
    Expect("HEV", 0.0),
    Expect("HP", 5.0),
    Link("H", "X CP"),
    Expect("H", 3.0),
    Link("H", "X CP"),
    Expect("H", 4.0),
    X(4.0),
    Expect("H", 5.0),
];

async fn connected_ioc() -> std::sync::Arc<PvDatabase> {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.process_cp_links_on_connect().await;
    db
}

fn val(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

#[epics_macros_rs::epics_test]
async fn dbpf_h_inpa_x_cp_processes_h_once_and_dbpf_h_inpa_x_drops_its_edge() {
    let db = connected_ioc().await;
    for (i, step) in C_SEQUENCE.iter().enumerate() {
        match *step {
            Link(holder, text) => {
                db.put_record_field_from_ca(
                    holder,
                    "INPA",
                    EpicsValue::String(PvString::from(text)),
                )
                .await
                .unwrap();
            }
            X(v) => {
                db.put_record_field_from_ca("X", "VAL", EpicsValue::Double(v))
                    .await
                    .unwrap();
            }
            Expect(pv, want) => assert_eq!(val(&db, pv), want, "step {i}: {pv}"),
        }
    }
}

/// A second setup and connect pass replaces every edge: one post still
/// processes the holder once.
#[epics_macros_rs::epics_test]
async fn a_repeated_connect_pass_replaces_each_edge() {
    let db = connected_ioc().await;
    let edges = db.get_cp_targets("X").len();
    db.setup_cp_links().await;
    db.process_cp_links_on_connect().await;
    assert_eq!(db.get_cp_targets("X").len(), edges);
    let before = val(&db, "HP");
    db.put_record_field_from_ca("X", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert_eq!(val(&db, "HP"), before + 1.0);
}
