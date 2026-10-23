//! `PvDatabase::restore_target` reads a name as `SR_get_array_info`'s
//! `dbNameToAddr` does (dbAccess.c:660-705): the record runs to the first
//! `.`, the field is the identifier after it, a `$` makes a string or link
//! field a `DBF_CHAR` array, and the rest is a filter it does not read.

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{DbFieldType, DbfCode};

async fn db() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    db.add_record("W", Box::new(WaveformRecord::new(8, DbFieldType::Double)))
        .await
        .unwrap();
    db.add_record("X", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db
}

fn elements(db: &PvDatabase, name: &str) -> Option<u32> {
    db.restore_target(name).map(|t| t.elements)
}

#[epics_macros_rs::epics_test]
async fn a_filter_after_the_field_is_not_read() {
    let db = db().await;
    assert_eq!(elements(&db, r#"W.VAL{"arr":{"s":1}}"#), Some(8));
}

#[epics_macros_rs::epics_test]
async fn a_dot_inside_the_filter_does_not_split_the_name() {
    let db = db().await;
    assert_eq!(elements(&db, r#"W.VAL{"d":1.5}"#), Some(8));
    assert_eq!(elements(&db, r#"W.{"d":1.5}"#), Some(8));
}

/// dbAccess.c:688-699: a string's `field_size`, a link's `PVLINK_STRINGSZ`,
/// and `S_dbLib_fieldNotFound` for anything else.
#[epics_macros_rs::epics_test]
async fn a_dollar_reads_the_field_as_chars() {
    let db = db().await;
    let desc = db.restore_target("X.DESC$").unwrap();
    assert_eq!((desc.dbf, desc.elements), (DbfCode::Char, 41));
    assert_eq!(elements(&db, "X.DOL$"), Some(1024));
    assert_eq!(elements(&db, "X.VAL$"), None);
    assert_eq!(elements(&db, "X.DESC"), Some(1));
}
