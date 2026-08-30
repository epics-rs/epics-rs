//! A record whose DTYP resolved to nothing does not process — it goes PACT.
//!
//! ```c
//! if ((pdset==NULL) || (pdset->write_ao==NULL)) {
//!     prec->pact=TRUE;
//!     recGblRecordError(S_dev_missingSup, prec, "write_ao");
//!     return(S_dev_missingSup);
//! }
//! ```
//! (`aoRecord.c:172-176`; the first statement of `process` in 20
//! `<rec>Record.c` files.)
//!
//! The port used to run the whole cycle instead. Measured against `softIoc`
//! R7.0.10 on `asyn`'s `testErrors` IOC, whose `asyn*` DTYPs are declared by
//! the DBD and registered by nobody: C left `testErrors:AoInt32` at `PACT 1`,
//! `STAT UDF`, `TIME <undefined>`; this port left it `PACT 0`, `STAT
//! NO_ALARM` and stamped, and printed none of C's 28 refusal lines.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

fn listen() -> Arc<Mutex<Vec<String>>> {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&heard);
    epics_base_rs::runtime::log::errlog_add_listener(move |m| {
        sink.lock().expect("sink").push(m.to_string());
    });
    heard
}

fn heard(sink: &Arc<Mutex<Vec<String>>>) -> String {
    epics_base_rs::runtime::log::errlog_flush();
    sink.lock().expect("sink").join("\n")
}

async fn ioc(db_text: &str) -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// The refusal, the PACT it takes, and the fact that the PACT is what stops
/// the second report — C prints one line per record however often the record
/// is scanned, because `dbProcess` turns every later entry away.
#[epics_macros_rs::epics_test]
async fn the_first_cycle_refuses_and_the_record_stays_active() {
    let sink = listen();
    let db = ioc(
        "record(ao, \"T:A\") { field(DTYP,\"asynInt32\") field(OUT,\"T:B\") }\n\
                  record(ai, \"T:B\") {}\n",
    )
    .await;

    db.process_record("T:A").await.unwrap();
    let after_one = heard(&sink);
    assert!(
        after_one.contains("recGblRecordError: write_ao Error (514,5) PV: T:A"),
        "C `aoRecord.c:174` through `recGbl.c:68-70`, got {after_one:?}"
    );
    assert_eq!(
        db.get_pv("T:A.PACT").unwrap(),
        EpicsValue::Char(1),
        "C `prec->pact = TRUE`, and nothing releases it"
    );

    db.process_record("T:A").await.unwrap();
    let after_two = heard(&sink);
    assert_eq!(
        after_two.matches("write_ao Error (514,5)").count(),
        1,
        "the PACT guard turns the second entry away before the refusal, got {after_two:?}"
    );
}

/// A record type C never tests `prec->dset` in keeps processing. `calc` has
/// no dset check anywhere, so a nonsense DTYP must not make it inert.
#[epics_macros_rs::epics_test]
async fn a_record_type_with_no_dset_check_still_processes() {
    let sink = listen();
    let db = ioc("record(calc, \"T:C\") { field(DTYP,\"asynInt32\") field(CALC,\"7\") }\n").await;

    db.process_record("T:C").await.unwrap();
    let log = heard(&sink);
    assert!(
        !log.contains("Error (514,5)"),
        "calcRecord.c `process` has no dset test, got {log:?}"
    );
    assert_eq!(
        db.get_pv("T:C.VAL").unwrap(),
        EpicsValue::Double(7.0),
        "the cycle ran"
    );
    assert_eq!(db.get_pv("T:C.PACT").unwrap(), EpicsValue::Char(0));
}
