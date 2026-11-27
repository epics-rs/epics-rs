//! A record attribute is a string, and C `dbGet` reads one through
//! `getAttrValue` with or without `$` in its name (`dbAccess.c:950-953`):
//! `DBR_STRING` and `DBR_CHAR`/`DBR_UCHAR` copy its text, every other
//! request type is `S_db_badDbrtype` (`dbAccess.c:865-895`).

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status::LINK_ALARM;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(ai, "AI") { field(DESC, "7") }
record(calc, "NUM") {
    field(INPA, "AI.NUM")
    field(CALC, "A")
}
record(calc, "DESC") {
    field(INPA, "AI.DESC")
    field(CALC, "A")
}
record(stringin, "LATE") { field(INP, "AI.LATE") }
"#;

async fn ioc() -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.put_record_type_attribute("ai", Some("NUM"), Some("5"))
        .unwrap();
    // Hidden by the declared `DESC`: `dbFindFieldPart` runs first.
    db.put_record_type_attribute("ai", Some("DESC"), Some("x"))
        .unwrap();
    db.ioc_init().await;
    db
}

/// `DBR_CHAR` and `DBR_UCHAR` copy the text with its NUL, `nRequest`
/// becoming `strlen + 1` (`dbAccess.c:879-892`).
#[epics_macros_rs::epics_test]
async fn an_attribute_reads_as_its_text_in_chars() {
    let db = ioc().await;
    let h = db.async_handle();
    let get = |name, dbr| h.db_get(name, dbr, 40, None).map(|s| s.value);
    assert_eq!(
        get("AI.NUM", DbFieldType::Char),
        Some(EpicsValue::CharArray(b"5\0".to_vec().into()))
    );
    assert_eq!(
        get("AI.NUM", DbFieldType::UChar),
        Some(EpicsValue::UCharArray(b"5\0".to_vec().into()))
    );
    assert_eq!(
        get("AI.RTYP", DbFieldType::Char),
        Some(EpicsValue::CharArray(b"ai\0".to_vec().into()))
    );
}

/// Every other request type is `S_db_badDbrtype` (`dbAccess.c:885-886`):
/// the text is never converted, even where it spells a number. A declared
/// field of the attribute's name is the field, converted as one.
#[epics_macros_rs::epics_test]
async fn an_attribute_refuses_a_numeric_read() {
    let db = ioc().await;
    let h = db.async_handle();
    let get = |name, dbr| h.db_get(name, dbr, 1, None).map(|s| s.value);
    assert_eq!(get("AI.NUM", DbFieldType::Double), None);
    assert_eq!(get("AI.NUM", DbFieldType::Long), None);
    assert_eq!(
        get("AI.NUM", DbFieldType::String),
        Some(EpicsValue::String("5".into()))
    );
    assert_eq!(
        get("AI.DESC", DbFieldType::Double),
        Some(EpicsValue::Double(7.0))
    );
}

fn alarm(db: &PvDatabase, rec: &str) -> (AlarmSeverity, u16) {
    let inst = db.get_record(rec).unwrap();
    let inst = inst.read();
    (inst.common.sevr, inst.common.stat)
}

/// A link reads the attribute through the same `getAttrValue`
/// (`dbDbLink.c:188`), so calc's `DBR_DOUBLE` fetch fails, `dbGetLink`
/// raises LINK/INVALID (`dbLink.c:338-339`), and `A` and `VAL` keep their
/// values (`calcRecord.c:120`). A link to the declared field reads it.
#[epics_macros_rs::epics_test]
async fn a_numeric_link_read_of_an_attribute_raises_link_invalid() {
    let db = ioc().await;
    db.process_record("NUM").await.unwrap();
    assert_eq!(alarm(&db, "NUM"), (AlarmSeverity::Invalid, LINK_ALARM));
    assert_eq!(db.get_pv("NUM.A").unwrap(), EpicsValue::Double(0.0));
    assert_eq!(db.get_pv("NUM.VAL").unwrap(), EpicsValue::Double(0.0));

    db.process_record("DESC").await.unwrap();
    assert_eq!(alarm(&db, "DESC"), (AlarmSeverity::NoAlarm, 0));
    assert_eq!(db.get_pv("DESC.VAL").unwrap(), EpicsValue::Double(7.0));
}

/// A link whose name no field or attribute answers at `iocInit` is a CA
/// link in C (`dbLink.c:118-130`), and rsrv's search re-runs
/// `dbGetAttributePart` (`camessage.c:2221`, `dbChannel.c:326`), so the
/// link reads an attribute `dbPutAttribute` adds later.
#[epics_macros_rs::epics_test]
async fn a_link_reads_an_attribute_added_after_it_resolved() {
    let db = ioc().await;
    // Unread, the link fails `readValue`.
    let _ = db.process_record("LATE").await;
    assert_eq!(alarm(&db, "LATE"), (AlarmSeverity::Invalid, LINK_ALARM));

    db.put_record_type_attribute("ai", Some("LATE"), Some("late"))
        .unwrap();
    db.process_record("LATE").await.unwrap();
    assert_eq!(
        db.get_pv("LATE.VAL").unwrap(),
        EpicsValue::String("late".into())
    );
    assert_eq!(alarm(&db, "LATE"), (AlarmSeverity::NoAlarm, 0));
}
