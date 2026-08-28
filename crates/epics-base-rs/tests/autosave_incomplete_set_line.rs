//! A save file states how much of its set it could not save, and both
//! ends of the port have to speak that statement.
//!
//! C writes `! <n> channel(s) not connected - or not all gets were
//! successful` under the header whenever a set saved with unreachable
//! members (`save_restore.c` `write_it` at `R6-0-20-g186f467`), and C's
//! restore reads the number back: it logs it, marks the set WARN, and
//! with `save_restoreIncompleteSetsOk` cleared refuses the restore
//! (`dbrestore.c:994-1010`). Omitting the line tells a C IOC the save
//! set was complete; parsing it as a PV line — which is what a leading
//! `!` used to do here — invents a PV named `!` and loses the statement.

use epics_base_rs::server::autosave::format::CompatMode;
use epics_base_rs::server::autosave::save_file::{
    SaveEntry, read_save_file, write_save_file_with_mode,
};
use epics_base_rs::server::autosave::save_set::restore_from_entries;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;

fn entry(name: &str, value: &str, connected: bool) -> SaveEntry {
    SaveEntry {
        pv_name: name.into(),
        value: value.into(),
        connected,
    }
}

/// The line is the file format, not the value encoding, so it is
/// written in both modes — a C IOC reads `Native` files too.
#[epics_macros_rs::epics_test]
async fn an_incomplete_set_declares_itself_in_both_modes() {
    let dir = tempfile::tempdir().unwrap();
    for (mode, name) in [
        (CompatMode::Native, "native.sav"),
        (CompatMode::CRead, "cread.sav"),
    ] {
        let path = dir.path().join(name);
        write_save_file_with_mode(
            &path,
            &[
                entry("PV1", "1.0", true),
                entry("PV2", "", false),
                entry("PV3", "", false),
            ],
            mode,
        )
        .await
        .unwrap();

        let text = epics_base_rs::runtime::fs::read_to_string(&path)
            .await
            .unwrap();
        assert!(
            text.contains("! 2 channel(s) not connected - or not all gets were successful"),
            "{name} must declare the two PVs it could not save; got: {text:?}"
        );
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with('#'), "header first");
        assert!(lines[1].starts_with('!'), "then the declaration: {lines:?}");
    }
}

/// A complete set says nothing, because C says nothing.
#[epics_macros_rs::epics_test]
async fn a_complete_set_writes_no_declaration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("all.sav");
    write_save_file_with_mode(&path, &[entry("PV1", "1.0", true)], CompatMode::Native)
        .await
        .unwrap();
    let text = epics_base_rs::runtime::fs::read_to_string(&path)
        .await
        .unwrap();
    assert!(!text.contains('!'), "nothing to declare; got: {text:?}");
}

/// Reading C's own file: the count survives and no PV named `!` is
/// invented from it.
#[epics_macros_rs::epics_test]
async fn a_c_written_incomplete_file_keeps_its_count_and_names_no_pv() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.sav");
    epics_base_rs::runtime::fs::write(
        &path,
        "# save/restore V5.1\tAutomatically generated - DO NOT MODIFY - 260826-120000\n\
         ! 7 channel(s) not connected - or not all gets were successful\n\
         IOC:setpoint 42.5\n\
         #IOC:gone Search Issued\n\
         <END>\n",
    )
    .await
    .unwrap();

    let contents = read_save_file(&path).await.unwrap().expect("has <END>");
    assert_eq!(contents.not_connected, 7);
    assert!(
        !contents.entries.iter().any(|e| e.pv_name == "!"),
        "the declaration must not become a PV: {:?}",
        contents.entries
    );
    assert!(
        contents.malformed.is_empty(),
        "nor a malformed line: {:?}",
        contents.malformed
    );
    // C's own marker for a PV it never connected to, which C's asVerify
    // counts and this reader used to skip as an ordinary comment.
    let gone = contents
        .entries
        .iter()
        .find(|e| e.pv_name == "IOC:gone")
        .expect("the unsaved PV must still be visible");
    assert!(!gone.connected);
}

/// Round trip: what this writer declares is what this reader reads.
#[epics_macros_rs::epics_test]
async fn the_declaration_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rt.sav");
    write_save_file_with_mode(
        &path,
        &[entry("PV1", "1.0", true), entry("PV2", "", false)],
        CompatMode::CRead,
    )
    .await
    .unwrap();

    let contents = read_save_file(&path).await.unwrap().expect("has <END>");
    assert_eq!(contents.not_connected, 1);
    assert_eq!(
        contents.entries.iter().filter(|e| !e.connected).count(),
        1,
        "the aggregate and the per-PV markers must agree"
    );
}

/// The restore surfaces it, which for a C-written file is the only way
/// an operator learns the set was incomplete.
#[epics_macros_rs::epics_test]
async fn restore_reports_what_the_file_could_not_save() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.sav");
    epics_base_rs::runtime::fs::write(
        &path,
        "# save/restore V5.1\tgenerated\n\
         ! 3 channel(s) not connected - or not all gets were successful\n\
         PV1 42.5\n\
         <END>\n",
    )
    .await
    .unwrap();

    let db = PvDatabase::new();
    db.add_record("PV1", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();

    let result = restore_from_entries(&db, &path).await.unwrap();
    assert_eq!(result.not_connected_at_save, 3);
    assert_eq!(result.restored, 1);
    assert!(
        result.not_found.is_empty(),
        "the declaration must not be looked up as a PV: {:?}",
        result.not_found
    );
}
