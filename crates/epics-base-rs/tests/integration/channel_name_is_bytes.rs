//! A channel name is bytes: `dbChannelTest`/`dbChannelCreate` hand the
//! record part to `dbFindRecordPart`, which hashes it as bytes, so a client
//! naming `nu\xe9b.DESC` reaches the Latin-1 record and the UTF-8 spelling
//! of the same text does not. softIoc 7.0.10 on this record:
//!
//! ```text
//! dbgf "nu\xe9b.DESC"  -> DBF_STRING: "d\xe9sc"
//! ```

use std::collections::HashMap;

use epics_base_rs::server::database::PvEntry;
use epics_base_rs::server::database::filters::db_channel_create;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

#[epics_macros_rs::epics_test]
async fn a_latin1_channel_name_resolves_by_its_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nu.db");
    std::fs::write(
        &path,
        b"record(ai, \"nu\xe9b\") {\n  field(DESC, \"d\xe9sc\")\n}\n",
    )
    .expect("write the .db");
    let (db, _) = IocBuilder::new()
        .db_file(path.to_str().unwrap(), &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    for name in [&b"nu\xe9b"[..], b"nu\xe9b.DESC", b"nu\xe9b.DESC{}"] {
        assert!(db.has_name(name).await, "{name:?}");
    }
    assert!(matches!(
        db.find_entry(b"nu\xe9b.DESC").await,
        Some(PvEntry::Record(_))
    ));
    let channel = db_channel_create(&db, b"nu\xe9b.DESC").expect("dbChannelCreate");
    assert_eq!(&channel.name.record[..], b"nu\xe9b");
    match db.get_pv(b"nu\xe9b.DESC") {
        Ok(EpicsValue::String(s)) => assert_eq!(s.as_bytes(), b"d\xe9sc"),
        other => panic!("{other:?}"),
    }

    assert!(!db.has_name("nu\u{e9}b").await, "UTF-8 é is not 0xE9");
    assert!(db.find_entry("nu\u{e9}b.DESC").await.is_none());
}
