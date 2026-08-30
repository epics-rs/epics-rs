//! A record whose DTYP names device support nobody registered says so.
//!
//! ```c
//! if(!(pdset = (aidset *)(prec->dset))) {
//!     recGblRecordError(S_dev_noDSET, prec, "ai: init_record");
//!     return(S_dev_noDSET);
//! }
//! ```
//! (`aiRecord.c:105-109`, and the same three lines in 21 more
//! `<rec>Record.c` files.) `M_devSup` has no linked symbol table, so
//! `errSymLookup` renders `S_dev_noDSET` as `Error (514,3)`.
//!
//! Measured on `softIoc` R7.0.10 through `scripts/compat-smoke.sh`: 211 of
//! these lines over 6 startup scripts, against none from this port. The PV
//! sets of those same 6 cases are identical, so the records exist on both
//! sides and the missing line is the whole difference at init.
//!
//! The `IocBuilder` route reported nothing at all — not even the port's own
//! `warning:` — which is why both tests here drive that route.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;

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

/// The record type needs a dset, so C's line is owed — and the record is
/// still built, exactly as C builds it.
#[epics_macros_rs::epics_test]
async fn a_record_type_that_needs_a_dset_reports_the_missing_one() {
    let sink = listen();
    let db = ioc("record(ai, \"T:A\") { field(DTYP,\"asynInt32\") }\n").await;

    let log = heard(&sink);
    assert!(
        log.contains("recGblRecordError: ai: init_record Error (514,3) PV: T:A"),
        "C `aiRecord.c:107` through `recGbl.c:68-70`, got {log:?}"
    );
    assert!(
        db.get_pv("T:A.VAL").is_ok(),
        "C reports and keeps the record; `doInitRecord0` discards the status"
    );
}

/// A record type C never tests `prec->dset` in stays silent. The table is
/// the gate, so a `calc` with a nonsense DTYP earns the port's own warning
/// and nothing that claims to be C's.
#[epics_macros_rs::epics_test]
async fn a_record_type_that_needs_no_dset_stays_silent() {
    let sink = listen();
    let _db = ioc("record(calc, \"T:C\") { field(DTYP,\"asynInt32\") }\n").await;

    let log = heard(&sink);
    assert!(
        !log.contains("init_record Error (514,3)"),
        "calcRecord.c has no dset check at all, got {log:?}"
    );
}

/// `aai` and `aao` test `dset` ABOVE `init_record`'s `if (pass == 0) return 0`
/// (`aaiRecord.c:118-123`, `aaoRecord.c:125-131`), so both of `initDatabase`'s
/// calls report and softIoc writes the line twice for one record. Measured on
/// R7.0.10: two `recGblRecordError: aao: init_record Error (514,3) PV:
/// testErrors:AaoInt8Out` lines for a single record.
#[epics_macros_rs::epics_test]
async fn the_two_array_types_report_once_per_init_pass() {
    let sink = listen();
    let _db = ioc(concat!(
        "record(aao, \"T:AAO\") { field(DTYP,\"nobodyRegisteredThis\") }\n",
        "record(aai, \"T:AAI\") { field(DTYP,\"nobodyRegisteredThis\") }\n",
        "record(ao,  \"T:AO\")  { field(DTYP,\"nobodyRegisteredThis\") }\n",
    ))
    .await;

    let log = heard(&sink);
    let count = |needle: &str| log.matches(needle).count();
    assert_eq!(
        count("aao: init_record Error (514,3) PV: T:AAO"),
        2,
        "C `aaoRecord.c:125-131` is above the pass gate, got {log:?}"
    );
    assert_eq!(
        count("aai: init_record Error (514,3) PV: T:AAI"),
        2,
        "C `aaiRecord.c:118-123` is above the pass gate, got {log:?}"
    );
    assert_eq!(
        count("ao: init_record Error (514,3) PV: T:AO"),
        1,
        "C `aoRecord.c` tests dset below `if (pass == 0) return 0`, got {log:?}"
    );
}
