//! `SELM` put at runtime moves the NVL gate: C reads NVL only while SELM is
//! Specified, whatever it was at load. softIoc R7.0.10, `caput SELM n` then
//! `caput PROC 1` per step, `NV` a Passive `calc` counting its own processing,
//! INPA..INPC the constants 1,2,3:
//!
//! ```text
//! SL (NVL "NV PP")        SELM NV VAL SELN STAT SEVR
//!                          0   1   2    1  NO    NO
//!                          1   1   3    2  NO    NO
//!                          0   2   3    2  NO    NO
//!                          2   2   1    0  NO    NO
//!                          0   3   nan  3  UDF   INVALID
//!                          3   3   2    3  NO    NO
//!                          0   4   nan  4  UDF   INVALID
//! SD (NVL "NOEXIST.VAL")  SELM VAL STAT SEVR
//!                          0   0   LINK INVALID
//!                          1   3   NO   NO
//!                          0   3   LINK INVALID
//!                          2   1   NO   NO
//!                          0   1   LINK INVALID
//! ```

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(calc, "NV") { field(CALC, "VAL+1") field(VAL, "0") }
record(sel, "SL") { field(NVL, "NV PP") field(SELM, "Specified")
    field(INPA, "1") field(INPB, "2") field(INPC, "3") }
record(sel, "SD") { field(NVL, "NOEXIST.VAL") field(SELM, "Specified")
    field(INPA, "1") field(INPB, "2") field(INPC, "3") }
"#;

const UDF: i16 = 17;
const LINK: i16 = 14;
const INVALID: i16 = 3;

async fn step(db: &PvDatabase, rec: &str, selm: i16) {
    db.put_record_field_from_ca(rec, "SELM", EpicsValue::Short(selm))
        .await
        .unwrap();
    let mut visited = ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

#[epics_macros_rs::epics_test]
async fn a_live_nvl_is_read_only_while_selm_is_specified() {
    let db = build().await;
    let rows: [(i16, f64, f64, f64, i16, i16); 7] = [
        (0, 1.0, 2.0, 1.0, 0, 0),
        (1, 1.0, 3.0, 2.0, 0, 0),
        (0, 2.0, 3.0, 2.0, 0, 0),
        (2, 2.0, 1.0, 0.0, 0, 0),
        (0, 3.0, f64::NAN, 3.0, UDF, INVALID),
        (3, 3.0, 2.0, 3.0, 0, 0),
        (0, 4.0, f64::NAN, 4.0, UDF, INVALID),
    ];
    for (i, (selm, nv, val, seln, stat, sevr)) in rows.into_iter().enumerate() {
        step(&db, "SL", selm).await;
        let at = format!("step {i} SELM={selm}");
        assert_eq!(num(&db, "NV"), nv, "{at}: NV reads");
        let got = num(&db, "SL");
        assert!(
            got == val || (got.is_nan() && val.is_nan()),
            "{at}: VAL {got}"
        );
        assert_eq!(num(&db, "SL.SELN"), seln, "{at}: SELN");
        assert_eq!(
            db.get_pv("SL.STAT").unwrap(),
            EpicsValue::Short(stat),
            "{at}: STAT"
        );
        assert_eq!(
            db.get_pv("SL.SEVR").unwrap(),
            EpicsValue::Short(sevr),
            "{at}: SEVR"
        );
    }
}

#[epics_macros_rs::epics_test]
async fn a_dead_nvl_alarms_only_while_selm_is_specified() {
    let db = build().await;
    let rows: [(i16, f64, i16, i16); 5] = [
        (0, 0.0, LINK, INVALID),
        (1, 3.0, 0, 0),
        (0, 3.0, LINK, INVALID),
        (2, 1.0, 0, 0),
        (0, 1.0, LINK, INVALID),
    ];
    for (i, (selm, val, stat, sevr)) in rows.into_iter().enumerate() {
        step(&db, "SD", selm).await;
        let at = format!("step {i} SELM={selm}");
        assert_eq!(num(&db, "SD"), val, "{at}: VAL");
        assert_eq!(
            db.get_pv("SD.STAT").unwrap(),
            EpicsValue::Short(stat),
            "{at}: STAT"
        );
        assert_eq!(
            db.get_pv("SD.SEVR").unwrap(),
            EpicsValue::Short(sevr),
            "{at}: SEVR"
        );
    }
}
