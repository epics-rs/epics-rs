//! A refused `"Async Soft Channel"` INP makes the dset's `read_xxx` return 2
//! without `devBiSoft.c:54`'s `udf = FALSE`; bi/mbbi/mbbiDirect assign UDF
//! only inside `if (status == 0)`, so the record stays UDF 1 INVALID/UDF.
//! longin re-derives on top of that return and clears it. softIoc
//! R7.0.10-146-g8f5015b66, one `.PROC` each.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;

const DB_TEXT: &str = r##"
record(bi, "R_BI") { field(DTYP,"Async Soft Channel") }
record(mbbi, "R_MBBI") { field(DTYP,"Async Soft Channel") }
record(mbbiDirect, "R_MBD") { field(DTYP,"Async Soft Channel") }
record(bi, "R_BI_CALC") { field(DTYP,"Async Soft Channel") field(INP,{calc:{expr:"1"}}) }
record(bi, "R_BI_HW") { field(DTYP,"Async Soft Channel") field(INP,"#C0 S0") }
record(longin, "R_LI") { field(DTYP,"Async Soft Channel") }
"##;

async fn proc_state(db: &epics_base_rs::server::database::PvDatabase, rec: &str) -> String {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
    let g = |f: &str| db.get_pv(format!("{rec}.{f}")).unwrap().text_g();
    format!("{} {} {}", g("UDF"), g("SEVR"), g("STAT"))
}

#[epics_macros_rs::epics_test]
async fn a_refused_inp_leaves_the_status_gated_inputs_undefined() {
    let db = IocBuilder::new()
        .db_string(DB_TEXT, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for rec in ["R_BI", "R_MBBI", "R_MBD", "R_BI_CALC", "R_BI_HW"] {
        let s = proc_state(&db, rec).await;
        // UDF 1, SEVR INVALID, STAT UDF_ALARM.
        assert_eq!(s, "1 3 17", "{rec}");
    }
    let s = proc_state(&db, "R_LI").await;
    assert!(s.starts_with("0 "), "R_LI: {s}");
}
