//! A record attribute is a string, and C `dbGet` reads one through
//! `getAttrValue` with or without `$` in its name (`dbAccess.c:950-953`):
//! `DBR_STRING` and `DBR_CHAR`/`DBR_UCHAR` copy its text, every other
//! request type is `S_db_badDbrtype` (`dbAccess.c:865-895`).

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"record(ai, "AI") { field(DESC, "7") }"#;

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
