//! A `{calc:…}` link's `major`, else `minor`, raises `LINK_ALARM` on the
//! record that reads it (`lnkCalc_getValue`, `lnkCalc.c:599-622`). Every
//! expectation is what softIoc (8f5015b66) printed for `dbpf REC.PROC 1` /
//! `dbgf` on these records.

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;

const DB: &str = r#"
record(ai, "RA") { field(INP, "{calc:{expr:'VAL+1', major:'VAL>2', minor:'VAL>1'}}") }
record(ai, "RB") { field(INP, "{calc:{expr:'A', args:[5], minor:'VAL>4'}}") }
record(ai, "RC") { field(INP, "{calc:{expr:'A', args:[5], major:'VAL<0'}}") }
"#;

/// (VAL, SEVR, STAT, AMSG) after one process.
async fn process(db: &PvDatabase, rec: &str) -> (f64, AlarmSeverity, u16, String) {
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .unwrap();
    let cell = db.get_record(rec).unwrap();
    let inst = cell.read();
    (
        db.get_pv(rec).unwrap().to_f64().unwrap(),
        inst.common.sevr,
        inst.common.stat,
        String::from(&inst.common.amsg),
    )
}

#[epics_macros_rs::epics_test]
async fn a_calc_link_read_raises_major_else_minor() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &Default::default())
        .unwrap()
        .build()
        .await
        .unwrap();
    assert_eq!(
        process(&db, "RA").await,
        (1.0, AlarmSeverity::NoAlarm, 0, String::new())
    );
    assert_eq!(
        process(&db, "RA").await,
        (2.0, AlarmSeverity::Minor, 14, "post_minor error".into())
    );
    assert_eq!(
        process(&db, "RA").await,
        (3.0, AlarmSeverity::Major, 14, "post_major error".into())
    );
    assert_eq!(
        process(&db, "RB").await,
        (5.0, AlarmSeverity::Minor, 14, "post_minor error".into())
    );
    assert_eq!(
        process(&db, "RC").await,
        (5.0, AlarmSeverity::NoAlarm, 0, String::new())
    );
}
