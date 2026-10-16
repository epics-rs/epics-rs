//! A runtime put to a `dbCommon` link field (FLNK, TSEL, SDIS) takes effect
//! on the record's next process, and a link text C refuses leaves the old
//! link. softIoc (R7.0.10-146): `dbpf X.FLNK Y` then `X.PROC` processes Y;
//! `dbpf X.SDIS D` (D 1 = DISV) disables X; `X.TSE -2`, `X.TSEL T.TIME`
//! stamps X with T's time; `X.FLNK "Y CP"` reads back `Y`; `X.SDIS
//! '{"zz":1}'` and `X.TSEL '#C1 S2'` are refused and keep the old link.

use epics_base_rs::types::WallTime;
use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, PvString};

const DB: &str = r#"
record(ao, "X") { field(VAL, "0") }
record(calc, "Y") { field(CALC, "VAL+1") }
record(ao, "D") { field(VAL, "1") }
record(ao, "T") { field(VAL, "0") }
"#;

fn text(v: &str) -> EpicsValue {
    EpicsValue::String(PvString::from(v))
}

fn read(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        v => v.to_f64().unwrap().to_string(),
    }
}

async fn put(db: &PvDatabase, field: &str, value: EpicsValue) -> bool {
    db.put_record_field_from_ca("X", field, value).await.is_ok()
}

#[epics_macros_rs::epics_test]
async fn flnk_tsel_and_sdis_put_at_runtime_act_on_the_next_process() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    assert!(put(&db, "FLNK", text("Y")).await);
    assert!(put(&db, "PROC", EpicsValue::Long(1)).await);
    assert_eq!(read(&db, "Y"), "1", "X's FLNK processes Y");
    assert!(put(&db, "FLNK", text("")).await);
    assert!(put(&db, "PROC", EpicsValue::Long(1)).await);
    assert_eq!(read(&db, "Y"), "1", "an empty FLNK processes nothing");
    assert!(put(&db, "FLNK", text("Y CP")).await);
    assert_eq!(read(&db, "X.FLNK"), "Y", "a forward link drops CP");

    let stamp = WallTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    db.get_record("T").unwrap().write().common.time = stamp;
    assert!(put(&db, "TSE", EpicsValue::Short(-2)).await);
    assert!(put(&db, "TSEL", text("T.TIME")).await);
    assert!(put(&db, "PROC", EpicsValue::Long(1)).await);
    assert_eq!(
        db.get_record("X").unwrap().read().common.time,
        stamp,
        "TSEL stamps X with T's time"
    );

    assert!(put(&db, "SDIS", text("D")).await);
    assert!(put(&db, "VAL", EpicsValue::Double(5.0)).await);
    assert_eq!(read(&db, "X"), "5");
    // menuAlarmStat DISABLE = 18.
    assert_eq!(read(&db, "X.STAT"), "18", "SDIS disables X");

    assert!(
        !put(&db, "SDIS", text(r#"{"zz":1}"#)).await,
        "an unknown JSON link type is refused"
    );
    assert!(
        !put(&db, "TSEL", text("#C1 S2")).await,
        "a VME_IO TSEL is refused"
    );
    assert_eq!(read(&db, "X.SDIS"), "D NPP NMS", "the refused SDIS keeps D");
    assert_eq!(
        read(&db, "X.TSEL"),
        "T.TIME NPP NMS",
        "the refused TSEL keeps T.TIME"
    );
}
