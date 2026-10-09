//! A `FLNK` processes the record its cached address names — C
//! `dbScanFwdLink` → `processTarget` on the link's `dbAddr` — so an alias
//! reaches its record, the Passive gate holds, and a put of `FLNK` moves the
//! next hop. Every expectation is softIoc's `dbgf` on the same database.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, PvString};
use std::collections::HashMap;

const DB: &str = r#"
record(calc, "FL") { field(CALC, "VAL+1") field(FLNK, "NICK") }
record(calc, "REAL") { field(CALC, "VAL+1") alias("NICK") }
record(calc, "EVT") { field(SCAN, "Event") field(EVNT, "77") field(CALC, "VAL+1") }
record(calc, "OTHER") { field(CALC, "VAL+1") }
"#;

/// softIoc: REAL 1 through the alias; after `FLNK EVT` the Event target stays
/// 0; after `FLNK OTHER`, OTHER 1 and REAL still 1.
#[epics_macros_rs::epics_test]
async fn a_forward_link_follows_its_address_and_a_put_moves_it() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let val = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    db.process_record("FL").await.unwrap();
    assert_eq!(val("REAL"), 1.0);
    for (target, check, want) in [("EVT", "EVT", 0.0), ("OTHER", "OTHER", 1.0)] {
        db.put_record_field_from_ca("FL", "FLNK", EpicsValue::String(PvString::from(target)))
            .await
            .unwrap();
        db.process_record("FL").await.unwrap();
        assert_eq!(val(check), want, "FLNK {target}");
    }
    assert_eq!(val("REAL"), 1.0);
}
