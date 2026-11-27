//! An alias name is the bytes the `.db` spelled, in both of C's alias forms:
//! the record-body `alias("name")` and the file-scope `alias("record",
//! "name")` (`dbLexRoutines.c` `dbRecordAlias` / `dbAlias`), each of which
//! hands the token to `dbCreateAlias` unconverted.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;

const DB: &[u8] = b"record(ai, \"caf\xe9\") {
    alias(\"in\xe9\")
    field(VAL, \"3\")
}
alias(\"caf\xe9\", \"out\xe9\")
";

#[epics_macros_rs::epics_test]
async fn both_alias_forms_keep_a_latin1_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("alias.db");
    std::fs::write(&path, DB).expect("write the .db");
    let (db, _) = IocBuilder::new()
        .db_file(path.to_str().unwrap(), &HashMap::new())
        .expect("the .db loads")
        .build()
        .await
        .unwrap();

    for alias in [&b"in\xe9"[..], b"out\xe9"] {
        assert_eq!(
            db.resolve_alias(alias).as_deref(),
            Some(&b"caf\xe9"[..]),
            "{alias:?} names caf\\xe9"
        );
    }
}
