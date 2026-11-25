//! calcout POVL is the OVAL `monitor()` last posted (`calcoutRecord.c`
//! `if (prec->povl != prec->oval) { ...; prec->povl = prec->oval; }`), an
//! ASL0 DBF_DOUBLE a client reads and writes. The port kept `povl` but served
//! no POVL read, so a read answered 0 where C answers OVAL.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

#[epics_macros_rs::epics_test]
async fn povl_reads_the_oval_the_cycle_posted() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(calcout, "C") { field(CALC, "5") field(OCAL, "7") field(DOPT, "Use OCAL") field(POVL, "3") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    // `init_record` sets `povl = oval` (calcoutRecord.c:220).
    assert_eq!(db.get_pv("C.POVL").unwrap(), EpicsValue::Double(0.0));
    db.process_record("C").await.unwrap();
    assert_eq!(db.get_pv("C.OVAL").unwrap(), EpicsValue::Double(7.0));
    assert_eq!(db.get_pv("C.POVL").unwrap(), EpicsValue::Double(7.0));
    db.put_pv("C.POVL", EpicsValue::Double(9.0)).await.unwrap();
    let rec = db.get_record("C").unwrap();
    assert_eq!(
        rec.read().record.get_field("POVL"),
        Some(EpicsValue::Double(9.0))
    );
}
