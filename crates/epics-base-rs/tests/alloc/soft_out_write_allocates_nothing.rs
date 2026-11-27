//! A soft output's OUT write allocates nothing per cycle once warm: C
//! `dbPutLink(&prec->out, dbrType, &prec->oval, 1)` hands `dbPut` the
//! record's own field (`devAoSoft.c:39`, `devSoSoft.c:38`) and copies
//! nothing. Counted on the scan thread by the alloc binary's allocator.

#![cfg(all(exec_backend, target_os = "linux"))]

use std::collections::HashMap;
use std::time::Duration;

use alloc_count::named::{self, Name};
use epics_base_rs::server::ioc_builder::IocBuilder;

/// The 10 Hz scan thread.
static SCAN_THREAD: [Name; 1] = [Name::Exact("scan-0.1")];

async fn out_writes_allocate_nothing(db_text: &str, counter: &str) {
    let db = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let ticks = || db.get_pv(counter).unwrap().to_f64().unwrap();
    let owner = epics_base_rs::server::scan::ScanOwner::start(db.clone());
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1000)).await;

    let before = ticks();
    let window = named::open(&SCAN_THREAD);
    epics_base_rs::runtime::task::sleep(Duration::from_millis(2000)).await;
    let [scan] = window.close();
    let ran = ticks() - before;
    drop(owner);

    assert!(ran >= 10.0, "only {ran} cycles ran in the counted window");
    assert_eq!(
        (scan.allocs, scan.frees),
        (0, 0),
        "allocations and frees on scan-0.1 over {ran} cycles"
    );
}

#[epics_macros_rs::epics_test]
async fn ao_out_write_allocates_nothing() {
    out_writes_allocate_nothing(
        r#"
record(calc, "TICK") { field(SCAN, ".1 second") field(CALC, "VAL+1") }
record(ao, "SRC") { field(SCAN, ".1 second") field(VAL, "2.5") field(OUT, "DST NPP") }
record(ao, "DST") {}
"#,
        "TICK",
    )
    .await;
}

#[epics_macros_rs::epics_test]
async fn stringout_out_write_allocates_nothing() {
    out_writes_allocate_nothing(
        r#"
record(calc, "TICK") { field(SCAN, ".1 second") field(CALC, "VAL+1") }
record(stringout, "SRC") { field(SCAN, ".1 second") field(VAL, "forty") field(OUT, "DST NPP") }
record(stringout, "DST") {}
"#,
        "TICK",
    )
    .await;
}
