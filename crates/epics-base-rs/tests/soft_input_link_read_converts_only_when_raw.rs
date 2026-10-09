//! A DB-link read into a Soft Channel input returns 2 ("don't convert"): VAL
//! takes the source and RVAL stays; Raw Soft Channel lands the source in RVAL
//! and runs the RVAL->VAL convert. softIoc R7.0.10.1, `INP "SRC"` (ao 5),
//! RVAL preset 3 (bi 0), `caput PROC 1`:
//!
//! ```text
//!        VAL      RVAL
//! AIR     5        5
//! AIS     5        3
//! BIR     1        5
//! BIS     5        0
//! MBR     65535    5     (no state matches RVAL 5)
//! MBS     5        3
//! MDR     5        5
//! MDS     5        3
//! ```

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;

const DB: &str = r#"
record(ao, "SRC") { field(VAL, "5") }
record(ai, "AIR") { field(DTYP, "Raw Soft Channel") field(INP, "SRC") field(RVAL, "3") }
record(ai, "AIS") { field(DTYP, "Soft Channel") field(INP, "SRC") field(RVAL, "3") }
record(bi, "BIR") { field(DTYP, "Raw Soft Channel") field(INP, "SRC") field(RVAL, "0") field(ZNAM, "zero") field(ONAM, "one") }
record(bi, "BIS") { field(DTYP, "Soft Channel") field(INP, "SRC") field(RVAL, "0") field(ZNAM, "zero") field(ONAM, "one") }
record(mbbi, "MBR") { field(DTYP, "Raw Soft Channel") field(INP, "SRC") field(RVAL, "3") field(NOBT, "4")
    field(ZRST, "a") field(ONST, "b") field(TWST, "c") field(THST, "d")
    field(ZRVL, "0") field(ONVL, "1") field(TWVL, "2") field(THVL, "3") }
record(mbbi, "MBS") { field(DTYP, "Soft Channel") field(INP, "SRC") field(RVAL, "3") field(NOBT, "4")
    field(ZRST, "a") field(ONST, "b") field(TWST, "c") field(THST, "d")
    field(ZRVL, "0") field(ONVL, "1") field(TWVL, "2") field(THVL, "3") }
record(mbbiDirect, "MDR") { field(DTYP, "Raw Soft Channel") field(INP, "SRC") field(RVAL, "3") field(NOBT, "8") }
record(mbbiDirect, "MDS") { field(DTYP, "Soft Channel") field(INP, "SRC") field(RVAL, "3") field(NOBT, "8") }
"#;

async fn val_rval(db: &PvDatabase, rec: &str) -> (f64, f64) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
    let f = |field: &str| {
        db.get_pv(&format!("{rec}.{field}"))
            .unwrap()
            .to_f64()
            .unwrap()
    };
    (f("VAL"), f("RVAL"))
}

#[epics_macros_rs::epics_test]
async fn only_raw_soft_channel_converts_a_link_read() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for (rec, val, rval) in [
        ("AIR", 5.0, 5.0),
        ("AIS", 5.0, 3.0),
        ("BIR", 1.0, 5.0),
        ("BIS", 5.0, 0.0),
        ("MBR", 65535.0, 5.0),
        ("MBS", 5.0, 3.0),
        ("MDR", 5.0, 5.0),
        ("MDS", 5.0, 3.0),
    ] {
        assert_eq!(val_rval(&db, rec).await, (val, rval), "{rec} VAL/RVAL");
    }
}
