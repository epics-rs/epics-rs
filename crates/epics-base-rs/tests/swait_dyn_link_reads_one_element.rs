//! swait reads INxN and DOLN with `recDynLinkGet` out of an `rdlSCALAR` CA
//! buffer (`swaitRecord.c:357-358`, `recDynLink.c:539`), not `dbGetLink`: an
//! array source delivers element 0, and an empty one the zero the server fills
//! the buffer with, with no alarm.
//!
//! oracleSwaitIoc (calc R3-7), `WE` a DOUBLE waveform, `SW` CALC "A+1" INAN
//! "WE", `SWD` DOPT "Use DOL" DOLN "WE" OUTN "SWDO", each processed once per
//! phase:
//!
//! ```text
//! WE [5,6]  SW A=5 VAL=6   SWD DOLD=5 SWDO=5
//! WE []     SW A=0 VAL=1   SWD DOLD=0 SWDO=0   all NO_ALARM
//! ```
//!
//! Here each is processed until the phase's value shows: a link takes it on
//! the monitor after the put.

use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

const PV_OK: f64 = 0.0;

const DB: &str = r#"
record(waveform, "WE") { field(FTVL, "DOUBLE") field(NELM, "4") }
record(ao, "SWDO") { }
record(swait, "SW") { field(CALC, "A+1") field(INAN, "WE") }
record(swait, "SWD") { field(CALC, "1") field(DOPT, "Use DOL") field(DOLN, "WE")
                       field(OOPT, "Every Time") field(OUTN, "SWDO") }
"#;

fn field(db: &PvDatabase, rec: &str, f: &str) -> f64 {
    let r = db.get_record(rec).unwrap();
    let inst = r.read();
    inst.record.get_field(f).unwrap().to_f64().unwrap()
}

fn sevr(db: &PvDatabase, rec: &str) -> AlarmSeverity {
    db.get_record(rec).unwrap().read().common.sevr
}

/// Put WE, then process SW and SWD until SW.A and SWDO read `want`.
async fn phase(db: &PvDatabase, we: Vec<f64>, want: f64) -> [f64; 4] {
    db.put_record_field_from_ca_no_notify("WE", "VAL", EpicsValue::DoubleArray(we.into()))
        .await
        .unwrap();
    for _ in 0..400 {
        for rec in ["SW", "SWD"] {
            let mut visited = epics_base_rs::server::database::ProcStack::new();
            let _ = db.process_record_with_links(rec, &mut visited).await;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
        if field(db, "SW", "A") == want && field(db, "SWDO", "VAL") == want {
            break;
        }
    }
    [
        field(db, "SW", "A"),
        field(db, "SW", "VAL"),
        field(db, "SWD", "DOLD"),
        field(db, "SWDO", "VAL"),
    ]
}

#[epics_macros_rs::epics_test]
async fn swait_reads_element_0_and_the_zero_fill_of_an_empty_source() {
    let (db, _) = IocBuilder::new()
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let statuses = [("SW", "INAV"), ("SWD", "DOLV"), ("SWD", "OUTV")];
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while statuses.iter().any(|(r, f)| field(&db, r, f) != PV_OK)
        && std::time::Instant::now() < deadline
    {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    let full = phase(&db, vec![5.0, 6.0], 5.0).await;
    let empty = phase(&db, Vec::new(), 0.0).await;
    let alarms = [sevr(&db, "SW"), sevr(&db, "SWD")];
    assert_eq!(
        (full, empty, alarms),
        (
            [5.0, 6.0, 5.0, 5.0],
            [0.0, 1.0, 0.0, 0.0],
            [AlarmSeverity::NoAlarm; 2]
        ),
        "[SW.A, SW.VAL, SWD.DOLD, SWDO.VAL] per phase, then SW/SWD SEVR"
    );
}
