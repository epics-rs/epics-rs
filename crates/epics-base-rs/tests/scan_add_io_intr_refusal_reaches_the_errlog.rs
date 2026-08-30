//! `scanAdd`'s I/O Intr refusal is a `recGblRecordError`, not a sentence.
//!
//! ```c
//! if (precord->dset == NULL){
//!     recGblRecordError(-1, precord,
//!         "scanAdd: I/O Intr not valid (no DSET) ");
//!     precord->scan = menuScanPassive;
//!     return;
//! }
//! ```
//! (`dbScan.c:272-277`; `:281-284` is the same shape with
//! `"(no get_ioint_info)"`.)
//!
//! Two things the port's own `eprintln!` did not do. The line went to
//! `stderr` and not to the errlog, so an IOC forwarding to a log server
//! reported nothing; and it was worded `scanAdd: I/O Intr not valid (no
//! DSET), <name> set to Passive`, so a site or a test grepping
//! `recGblRecordError` — which is how every other record-level refusal is
//! found — matched nothing.
//!
//! `-1` is not a positive status, so C skips `errSymLookup` and the middle
//! `%s` of `"recGblRecordError: %s %s PV: %s\n"` is empty; with C's own
//! trailing space in the message that is the three spaces before `PV:`.
//! Measured on `softIoc` R7.0.10 through `scripts/compat-smoke.sh`:
//! `recGblRecordError: scanAdd: I/O Intr not valid (no DSET)   PV:
//! asyndevAiInt32A0`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// The refusal is written during `build()`, so the listener goes on first.
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

/// A record asking for I/O Intr with no device support at all: C's first
/// branch, byte for byte, and the demotion it announces.
#[epics_macros_rs::epics_test]
async fn a_record_with_no_dset_is_refused_through_recgbl_record_error() {
    let sink = listen();
    let db = ioc("record(ai, \"T:A\") { field(SCAN,\"I/O Intr\") }\n").await;

    let log = heard(&sink);
    assert!(
        log.contains("recGblRecordError: scanAdd: I/O Intr not valid (no DSET)   PV: T:A"),
        "C `dbScan.c:273-275` through `recGbl.c:68-70`, got {log:?}"
    );

    // C sets `precord->scan = menuScanPassive` on the same path, and says
    // nothing further about it — the SCAN field is the report.
    assert_eq!(
        db.get_pv("T:A.SCAN").expect("SCAN reads back"),
        EpicsValue::Enum(0),
        "the refused record is demoted to Passive"
    );
}
