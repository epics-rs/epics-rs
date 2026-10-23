//! A `.db` file is bytes: C `dbLex` reads it with `fgets` and
//! `dbRecordNameValidate` (`dbLexRoutines.c:1085`) refuses only control
//! bytes and the link punctuation, so a Latin-1 name loads under its
//! own bytes. softIoc 7.0.10 on this file:
//!
//! ```text
//! dbl                -> nu\xe9b, plain
//! dbpr "nu\xe9b"     -> DESC: d\xe9sc   NAME: nu\xe9b   VAL : 3.5
//! ```

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;

#[epics_macros_rs::epics_test]
async fn a_latin1_record_name_loads_under_its_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nu.db");
    std::fs::write(
        &path,
        b"record(ai, \"nu\xe9b\") {\n  field(VAL, \"3.5\")\n  field(DESC, \"d\xe9sc\")\n}\n\
          record(stringin, \"plain\") {\n}\n",
    )
    .expect("write the .db");

    let (db, _) = IocBuilder::new()
        .db_file(path.to_str().unwrap(), &HashMap::new())
        .expect("a non-UTF-8 .db loads")
        .build()
        .await
        .unwrap();

    let mut names: Vec<Vec<u8>> = db
        .all_record_names()
        .iter()
        .map(|n| n.as_bytes().to_vec())
        .collect();
    names.sort();
    assert_eq!(names, [b"nu\xe9b".to_vec(), b"plain".to_vec()]);

    let rec = db.get_record(b"nu\xe9b").expect("found by its bytes");
    let desc = rec.read().common.text.desc.clone();
    assert_eq!(desc.as_bytes(), b"d\xe9sc");
    assert!(db.get_record("nu\u{e9}b").is_none(), "UTF-8 é is not 0xE9");
}
