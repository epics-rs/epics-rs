//! A swait without OUTN whose output fires runs its forward link after the
//! cycle's posts, as its OUTN path's put callback does. C's runs inside
//! `schedOutput` (swaitRecord.c:427 → 794), ahead of `monitor()` (`:475`), so
//! its FLNK target reads the last cycle's SEVR; that order is not copied.
//!
//! oracleSwaitIoc (calc R3-7-5), FN:Q and FN:QN processed once from UDF, each
//! FLNK a calc reading the swait's SEVR, FN:QN's OOPT Never:
//!
//! ```text
//! fn order QR 3 NO_ALARM  QNR 0 NO_ALARM
//! ```
//!
//! QR reads 0 here.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

const INVALID: f64 = 3.0;

const DB: &str = r#"
record(swait, "FN:Q") { field(CALC, "1") field(OOPT, "Every Time") field(FLNK, "FN:QR") }
record(calc, "FN:QR") { field(INPA, "FN:Q.SEVR") field(CALC, "A") }
record(swait, "FN:QN") { field(CALC, "1") field(OOPT, "Never") field(FLNK, "FN:QNR") }
record(calc, "FN:QNR") { field(INPA, "FN:QN.SEVR") field(CALC, "A") }
"#;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

#[epics_macros_rs::epics_test]
async fn the_forward_link_reads_the_cycles_alarm() {
    let ioc = IocBuilder::new()
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = std::sync::Arc::clone(ioc.database());
    assert!(ioc.run().await);
    let pre = (num(&db, "FN:Q.SEVR"), num(&db, "FN:QN.SEVR"));
    for rec in ["FN:Q", "FN:QN"] {
        db.put_record_field_from_ca(rec, "PROC", EpicsValue::Double(1.0))
            .await
            .unwrap();
    }
    assert_eq!(
        (
            pre,
            num(&db, "FN:QR"),
            num(&db, "FN:QR.UDF"),
            num(&db, "FN:QNR"),
            num(&db, "FN:QNR.UDF")
        ),
        ((INVALID, INVALID), 0.0, 0.0, 0.0, 0.0),
        "Q and QN SEVR before; QR and QNR (VAL, UDF) after one process each"
    );
}
