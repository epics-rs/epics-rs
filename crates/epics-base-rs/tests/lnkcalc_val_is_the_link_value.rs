//! `VAL` in a `{calc:…}` link reads C `clink->val` (`lnkCalc.c:61`): on a
//! put, the value put and then the `expr` result (`lnkCalc.c:663-680`); on
//! a read, the last result (`lnkCalc.c:591-592`). Every expectation is what
//! softIoc (8f5015b66) printed for `dbpf` / `dbgf` on these records.

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "LV:EXPR") { field(OUT, "{calc:{expr:'VAL>3', out:{state:'lv1'}}}") }
record(ao, "LV:MAJ") { field(OUT, "{calc:{out:{state:'lv2'}, major:'VAL>3'}}") }
record(ao, "LV:POST") { field(OUT, "{calc:{expr:'VAL+1', major:'VAL==6', out:{state:'lv3'}}}") }
record(longin, "LV:RB1") { field(INP, "{state:'lv1'}") }
record(ai, "LV:READ") { field(INP, "{calc:{expr:'VAL+1'}}") }
"#;

async fn ioc() -> std::sync::Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &Default::default())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

async fn process(db: &PvDatabase, rec: &str) {
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .unwrap();
}

async fn dbpf(db: &PvDatabase, rec: &str, value: f64) {
    db.put_pv(rec, EpicsValue::Double(value)).await.unwrap();
    process(db, rec).await;
}

fn alarm(db: &PvDatabase, rec: &str) -> (AlarmSeverity, String) {
    let cell = db.get_record(rec).unwrap();
    let inst = cell.read();
    (inst.common.sevr, String::from(&inst.common.amsg))
}

#[epics_macros_rs::epics_test]
async fn val_in_expr_is_the_value_put() {
    let db = ioc().await;
    dbpf(&db, "LV:EXPR", 5.0).await;
    process(&db, "LV:RB1").await;
    assert_eq!(db.get_pv("LV:RB1").unwrap().to_f64(), Some(1.0));
    dbpf(&db, "LV:EXPR", 2.0).await;
    process(&db, "LV:RB1").await;
    assert_eq!(db.get_pv("LV:RB1").unwrap().to_f64(), Some(0.0));
}

#[epics_macros_rs::epics_test]
async fn val_in_major_is_the_value_put() {
    let db = ioc().await;
    dbpf(&db, "LV:MAJ", 5.0).await;
    assert_eq!(
        alarm(&db, "LV:MAJ"),
        (AlarmSeverity::Major, "post_major error".into())
    );
    dbpf(&db, "LV:MAJ", 1.0).await;
    assert_eq!(alarm(&db, "LV:MAJ").0, AlarmSeverity::NoAlarm);
}

/// `expr` 'VAL+1' on 5 is 6, and `major` sees the 6.
#[epics_macros_rs::epics_test]
async fn val_in_major_is_the_expr_result() {
    let db = ioc().await;
    dbpf(&db, "LV:POST", 5.0).await;
    assert_eq!(
        alarm(&db, "LV:POST"),
        (AlarmSeverity::Major, "post_major error".into())
    );
    dbpf(&db, "LV:POST", 6.0).await;
    assert_eq!(alarm(&db, "LV:POST").0, AlarmSeverity::NoAlarm);
}

#[epics_macros_rs::epics_test]
async fn val_in_a_read_is_the_last_result() {
    let db = ioc().await;
    for want in [1.0, 2.0, 3.0] {
        process(&db, "LV:READ").await;
        assert_eq!(db.get_pv("LV:READ").unwrap().to_f64(), Some(want));
    }
}
