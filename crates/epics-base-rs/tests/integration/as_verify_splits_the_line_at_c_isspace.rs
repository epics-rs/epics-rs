//! asVerify splits a `.sav` line with `sscanf("%80s%c...")` (`verify.c:133`),
//! whose `%s` stops at, and skips, every C `isspace` byte, `\v` included.
//!
//! The expected trial restore file is C asVerify's (autosave 6.1, R7.0.10)
//! for the same `.sav` and record.

use std::collections::HashMap;

use epics_base_rs::server::autosave::verify::do_as_verify;
use epics_base_rs::server::ioc_builder::IocBuilder;

#[epics_macros_rs::epics_test]
async fn a_vertical_tab_separates_and_leads_a_line_as_in_c() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("v.sav");
    std::fs::write(&sav, "ao\x0b1.5\n\x0bao 2\n<END>\n").unwrap();
    let db = IocBuilder::new()
        .db_string(r#"record(ao, "ao") { field(VAL, "1.5") }"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    let restore = dir.path().join("v.asVerify");
    // C: 1 PV differed (`ao 2`), 0 not connected.
    assert_eq!(do_as_verify(&db, &sav, 1, Some(&restore)).await, 1);

    let mut trial = restore.into_os_string();
    trial.push("B");
    let text = std::fs::read_to_string(trial).unwrap();
    let body = text.split_once('\n').unwrap().1;
    assert_eq!(body, "ao 1.5\nao 1.5\n<END>\n");
}
