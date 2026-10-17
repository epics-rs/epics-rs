//! A loaded acalcout has allocated no array, so AMEM and PMEM read 0.
//!
//! `aCalcoutRecord.c` `calloc`s each buffer lazily — `cvt_dbaddr` when a
//! channel resolves on it (`:589-617`), `process` and `monitor` on the first
//! cycle (`:379-387`, `:972-979`) — and `init_record` allocates none. The
//! load settles every `SPC_DBADDR` field's channel capacity for the lock-free
//! `dbNameToAddr`, and that settlement must not count as fourteen resolves.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::types::EpicsValue;

#[epics_macros_rs::epics_test]
async fn a_loaded_acalcout_has_charged_no_array() {
    let db = IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::new()))
        .db_string(r#"record(acalcout, "M") {}"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    assert_eq!(db.get_pv("M.AMEM").unwrap(), EpicsValue::Long(0));
    assert_eq!(db.get_pv("M.PMEM").unwrap(), EpicsValue::Long(0));
    // The settled count is still the channel's: dbNameToAddr on AA is NELM.
    let cell = db.get_record("M").unwrap();
    assert_eq!(cell.no_elements("AA"), Some(1));
}

/// sscan resolves a positioner or detector name with C's `dbNameToAddr`
/// (`sscanRecord.c:2335`), which runs `cvt_dbaddr` and charges the buffer
/// before the record ever processes. The port's resolve takes no lock, so the
/// charge is owed to the next holder of the record — and AMEM is read under it.
#[epics_macros_rs::epics_test]
async fn a_lock_free_resolve_charges_the_array_it_names() {
    let db = IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::new()))
        .db_string(r#"record(acalcout, "M") {}"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let handle = db.async_handle();
    // The record is busy: the resolve cannot charge it in place.
    let cell = db.get_record("M").unwrap();
    let busy = cell.write();
    assert_eq!(handle.name_to_addr("M.AA"), Some(1));
    assert_eq!(handle.name_to_addr("M.AA"), Some(1));
    drop(busy);
    assert_eq!(db.get_pv("M.AMEM").unwrap(), EpicsValue::Long(8));
    assert_eq!(db.get_pv("M.PMEM").unwrap(), EpicsValue::Long(8));
    // A field with no buffer behind it charges nothing.
    assert_eq!(handle.name_to_addr("M.NELM"), Some(1));
    assert_eq!(db.get_pv("M.AMEM").unwrap(), EpicsValue::Long(8));
}
