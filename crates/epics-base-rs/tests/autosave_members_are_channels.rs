//! An autosave member is a CA channel in C (`ca_create_channel`,
//! `save_restore.c:1294`), so its name is `dbChannelCreate`'s: a `$` member
//! is read as its chars, a `{json}` member through its filters, and a name
//! C refuses is written unconnected. The port read members with `get_pv`,
//! which resolves neither, so every `$` and filtered member was lost.
//!
//! The expected file is C autosave 6.1's on R7.0.10 for the same request
//! file and records, after `dbpf e "v0"` and `dbpf ao 7`.

use std::collections::HashMap;

use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::save_set::{SaveSet, SaveSetConfig};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const REQ: &str = r#"x.NOSUCH
x.DESC$
e.VAL$
x.VAL$
ao.VAL{"dbnd":{"d":1}}
ao.VAL{"nosuch":{}}
ao.VAL
"#;

const C_SAV: &str = concat!(
    "! 3 channel(s) not connected - or not all gets were successful\n",
    "#x.NOSUCH Search Issued\n",
    "x.DESC$ ab\n",
    "e.VAL$ v0\n",
    // C writes the `$` member's empty buffer after the name's space.
    "#x.VAL$ \n",
    "ao.VAL{\"dbnd\":{\"d\":1}} 7\n",
    "#ao.VAL{\"nosuch\":{}} Search Issued\n",
    "ao.VAL 7\n",
    "<END>\n",
);

#[epics_macros_rs::epics_test]
async fn members_are_read_as_the_channels_c_creates() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("t.req"), REQ).unwrap();
    let db = IocBuilder::new()
        .db_string(
            r#"
record(ai, "x") { field(DESC, "ab") }
record(lso, "e") { field(SIZV, "8") }
record(ao, "ao") { field(VAL, "1.5") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.put_record_field_from_ca("e", "VAL", EpicsValue::String("v0".into()))
        .await
        .unwrap();
    db.put_record_field_from_ca("ao", "VAL", EpicsValue::Double(7.0))
        .await
        .unwrap();

    let set = SaveSet::new(SaveSetConfig {
        name: "t".into(),
        save_path: dir.path().join("t.sav"),
        request_file: Some("t.req".into()),
        request_pvs: Vec::new(),
        backup: BackupConfig::default(),
        macros: HashMap::new(),
        macro_string: String::new(),
        search_paths: vec![dir.path().to_path_buf()],
    })
    .await
    .unwrap();
    set.save_once(&db).await.unwrap();

    let text = std::fs::read_to_string(dir.path().join("t.sav")).unwrap();
    // The first line is the release and date header.
    let body = text.split_once('\n').unwrap().1;
    assert_eq!(body, C_SAV);
}
