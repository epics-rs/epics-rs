//! A `{calc:…}` TSEL or SDIS link is the record's own link, so its `VAL`
//! (`clink->val`) carries from one cycle's read to the next. softIoc
//! (8f5015b66) after three `dbpf REC.PROC 1`: `TS.TSE` 3, `DS.DISA` 3.

use epics_base_rs::server::database::ProcStack;
use epics_base_rs::server::ioc_builder::IocBuilder;

const DB: &str = r#"
record(ai, "TS") { field(TSEL, "{calc:{expr:'VAL+1'}}") }
record(ai, "DS") { field(SDIS, "{calc:{expr:'VAL+1'}}") field(DISV, "100") }
"#;

#[epics_macros_rs::epics_test]
async fn tse_and_disa_count_three_processes() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &Default::default())
        .unwrap()
        .build()
        .await
        .unwrap();
    for rec in ["TS", "DS"] {
        for _ in 0..3 {
            db.process_record_with_links(rec, &mut ProcStack::new())
                .await
                .unwrap();
        }
    }
    assert_eq!(db.get_pv("TS.TSE").unwrap().to_f64(), Some(3.0));
    assert_eq!(db.get_pv("DS.DISA").unwrap().to_f64(), Some(3.0));
}
