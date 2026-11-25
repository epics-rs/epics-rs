//! C `scanAdd`'s I/O Intr refusal names why (`dbScan.c:272-282`): no dset, or a
//! dset with no `get_ioint_info`. A soft device has a dset in C even where the
//! port runs it with no device object. Ground truth, softIoc R7.0.10 on these
//! three records:
//!
//! ```text
//! recGblRecordError: scanAdd: I/O Intr not valid (no get_ioint_info)  PV: BO_RAW
//! recGblRecordError: scanAdd: I/O Intr not valid (no get_ioint_info)  PV: BO_SOFT
//! recGblRecordError: scanAdd: I/O Intr not valid (no DSET)   PV: CALC
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush};
use epics_base_rs::server::ioc_builder::IocBuilder;

const DB: &str = r#"record(bo, "BO_RAW") { field(DTYP, "Raw Soft Channel") field(SCAN, "I/O Intr") }
record(bo, "BO_SOFT") { field(SCAN, "I/O Intr") }
record(calc, "CALC") { field(SCAN, "I/O Intr") }
"#;

#[epics_macros_rs::epics_test]
async fn scan_add_names_c_s_reason_for_each_record() {
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&heard);
    errlog_add_listener(move |m| sink.lock().unwrap().push(m.to_string()));
    let _db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    errlog_flush();
    let mut said: Vec<String> = heard
        .lock()
        .unwrap()
        .iter()
        .flat_map(|m| m.lines().map(str::to_string).collect::<Vec<_>>())
        .filter(|l| l.contains("scanAdd"))
        .collect();
    said.sort();
    assert_eq!(
        said,
        [
            "recGblRecordError: scanAdd: I/O Intr not valid (no DSET)   PV: CALC",
            "recGblRecordError: scanAdd: I/O Intr not valid (no get_ioint_info)  PV: BO_RAW",
            "recGblRecordError: scanAdd: I/O Intr not valid (no get_ioint_info)  PV: BO_SOFT",
        ]
    );
}
