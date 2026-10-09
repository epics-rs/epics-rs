//! `devAiSoftRaw` with an INP that never reads: `dbGetLink` fails, so
//! `aiRecord::process` skips `convert` — VAL and RVAL keep what they held and
//! the record goes LINK/INVALID. Values are softIoc 7.0.10.1-DEV (`caget`
//! after each `caput`); SEVR/STAT 3/14 are INVALID/LINK.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ai, "R0") { field(DTYP,"Raw Soft Channel") field(INP,"DEAD.VAL") field(RVAL,"10") field(LINR,"NO CONVERSION") }
record(ai, "R1") { field(DTYP,"Raw Soft Channel") field(INP,"DEAD.VAL") field(RVAL,"10") field(LINR,"LINEAR") field(ESLO,"2") field(EOFF,"1") }
record(ai, "R2") { field(DTYP,"Raw Soft Channel") field(INP,"DEAD.VAL") field(RVAL,"10") field(VAL,"77") field(LINR,"NO CONVERSION") }
record(bi, "B0") { field(DTYP,"Raw Soft Channel") field(INP,"DEAD.VAL") field(RVAL,"1") field(ZNAM,"z") field(ONAM,"o") }
record(mbbi, "M0") { field(DTYP,"Raw Soft Channel") field(INP,"DEAD.VAL") field(RVAL,"2") field(NOBT,"2") field(ZRVL,"0") field(ONVL,"1") field(TWVL,"2") field(ZRST,"a") field(ONST,"b") field(TWST,"c") }
record(mbbiDirect, "D0") { field(DTYP,"Raw Soft Channel") field(INP,"DEAD.VAL") field(RVAL,"5") field(NOBT,"4") }
"#;

async fn proc(db: &PvDatabase, r: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    let _ = db.process_record_with_links(r, &mut visited).await;
}

async fn state(db: &PvDatabase, r: &str) -> (f64, i32, String, String) {
    let num = |f: &str| db.get_pv(&format!("{r}.{f}")).unwrap().to_f64().unwrap();
    let s = |f: &str| db.get_pv(&format!("{r}.{f}")).unwrap().to_string();
    (num("VAL"), num("RVAL") as i32, s("SEVR"), s("STAT"))
}

#[epics_macros_rs::epics_test]
async fn a_dead_inp_skips_convert_and_alarms_link() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for (r, val) in [("R0", 0.0), ("R1", 0.0), ("R2", 77.0)] {
        proc(&db, r).await;
        let (v, rv, sevr, stat) = state(&db, r).await;
        assert_eq!((v, rv), (val, 10), "{r}");
        assert_eq!((sevr.as_str(), stat.as_str()), ("3", "14"), "{r}");
        db.put_record_field_from_ca(r, "RVAL", EpicsValue::Long(20))
            .await
            .unwrap();
        proc(&db, r).await;
        assert_eq!(state(&db, r).await.0, val, "{r}: RVAL put then process");
        db.put_record_field_from_ca(r, "VAL", EpicsValue::Double(33.0))
            .await
            .unwrap();
        proc(&db, r).await;
        assert_eq!(state(&db, r).await.0, 33.0, "{r}: VAL put then process");
    }
}

/// Of the other Raw dsets, bi and mbbi return the `dbGetLink` status like ai;
/// `devMbbiDirectSoftRaw` returns 0 whatever it was, so mbbiDirect converts.
#[epics_macros_rs::epics_test]
async fn only_the_raw_dsets_that_return_the_status_skip_convert() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for (r, val) in [("B0", 0.0), ("M0", 0.0), ("D0", 5.0)] {
        proc(&db, r).await;
        let (v, rv, sevr, stat) = state(&db, r).await;
        assert_eq!(v, val, "{r}");
        assert_eq!(
            rv,
            if r == "B0" {
                1
            } else if r == "M0" {
                2
            } else {
                5
            },
            "{r}"
        );
        assert_eq!((sevr.as_str(), stat.as_str()), ("3", "14"), "{r}");
    }
}
