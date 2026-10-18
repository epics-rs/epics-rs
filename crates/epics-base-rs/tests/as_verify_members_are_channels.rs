//! asVerify reads each member through `ca_create_channel` (`verify.c:143`),
//! so its name is `dbChannelCreate`'s: a `$` member is read as `DBR_CHAR`,
//! a `{json}` member through its filters with their element count, and a
//! name C refuses is not connected. The port read members with `get_pv`,
//! which resolves neither.
//!
//! The expected trial restore file is C asVerify's (autosave 6.1, R7.0.10)
//! for the same `.sav` and records, after `dbpf e "v0"` and `dbpf ao 7`.

use std::collections::HashMap;

use epics_base_rs::server::autosave::verify::do_as_verify;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ai, "x") { field(DESC, "ab") }
record(lso, "e") { field(SIZV, "8") }
record(ao, "ao") { field(VAL, "1.5") }
record(waveform, "w") { field(FTVL, "DOUBLE") field(NELM, "4") field(INP, [1, 2, 3, 4]) }
record(waveform, "s") { field(FTVL, "CHAR") field(NELM, "16") field(INP, [104, 105, 0]) }
"#;

const SAV: &str = concat!(
    "x.DESC$ ab\n",
    "e.VAL$ v1\n",
    "ao.VAL{\"dbnd\":{\"d\":1}} 7\n",
    "ao.VAL{\"nosuch\":{}} 7\n",
    "x.NOSUCH 1\n",
    "x.VAL$ 0\n",
    "w.VAL{\"arr\":{\"s\":0,\"e\":1}} @array@ { \"1\" \"5\" }\n",
    "w.VAL @array@ { \"1\" \"2\" \"3\" \"4\" }\n",
    "s.VAL$ hi\n",
    "s.VAL{\"arr\":{\"s\":0,\"e\":1}} @array@ { \"104\" \"105\" }\n",
    "ao.VAL 7\n",
    "x.DESC ab\n",
    "<END>\n",
);

const C_RESTORE: &str = concat!(
    "x.DESC$ ab\n",
    "e.VAL$ v0\n",
    "ao.VAL{\"dbnd\":{\"d\":1}} 7\n",
    "#'ao.VAL{\"nosuch\":{}}' not connected\n",
    "#'x.NOSUCH' not connected\n",
    "#'x.VAL$' not connected\n",
    "w.VAL{\"arr\":{\"s\":0,\"e\":1}} @array@ { \"1\" \"2\" }\n",
    "w.VAL @array@ { \"1\" \"2\" \"3\" \"4\" }\n",
    "#'s.VAL$' not connected\n",
    // C writes `hi104`: its 2-element DBR_CHAR buffer has no NUL, and
    // `strlen` runs on into the file's elements (verify.c:394-396).
    "s.VAL{\"arr\":{\"s\":0,\"e\":1}} hi\n",
    "ao.VAL 7\n",
    "x.DESC ab\n",
    "<END>\n",
);

#[epics_macros_rs::epics_test]
async fn members_are_read_as_the_channels_c_creates() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("v.sav");
    std::fs::write(&sav, SAV).unwrap();
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
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

    let restore = dir.path().join("v.asVerify");
    // C: 3 PVs differed, 4 not connected.
    assert_eq!(do_as_verify(&db, &sav, 1, Some(&restore)).await, 7);

    // 4 not connected is not under 8 checked / 2, so only the trial file.
    let mut trial = restore.into_os_string();
    trial.push("B");
    let text = std::fs::read_to_string(trial).unwrap();
    // The first line is asVerify's own header.
    let body = text.split_once('\n').unwrap().1;
    assert_eq!(body, C_RESTORE);
}
