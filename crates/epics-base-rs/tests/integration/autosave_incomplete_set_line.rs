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

use epics_base_rs::server::autosave::save_file::{SaveEntry, read_save_file, write_save_file};
use epics_base_rs::server::autosave::save_set::restore_from_entries;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;

fn entry(name: &str, value: &str, connected: bool) -> SaveEntry {
    SaveEntry {
        pv_name: name.into(),
        value: value.into(),
        connected,
        array_fprintfs: Vec::new(),
    }
}

/// The declaration sits right under the header.
#[epics_macros_rs::epics_test]
async fn an_incomplete_set_declares_itself() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("incomplete.sav");
    write_save_file(
        &path,
        &[
            entry("PV1", "1.0", true),
            entry("PV2", "", false),
            entry("PV3", "", false),
        ],
    )
    .await
    .unwrap();

    let text = epics_base_rs::runtime::fs::read_to_string(&path)
        .await
        .unwrap();
    assert!(
        text.contains("! 2 channel(s) not connected - or not all gets were successful"),
        "the file must declare the two PVs it could not save; got: {text:?}"
    );
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with('#'), "header first");
    assert!(lines[1].starts_with('!'), "then the declaration: {lines:?}");
}

/// A complete set says nothing, because C says nothing.
#[epics_macros_rs::epics_test]
async fn a_complete_set_writes_no_declaration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("all.sav");
    write_save_file(&path, &[entry("PV1", "1.0", true)])
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
    let line = contents.error_message.as_ref().expect("the `!` line");
    assert_eq!(line.boot_count(), 7);
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
    write_save_file(&path, &[entry("PV1", "1.0", true), entry("PV2", "", false)])
        .await
        .unwrap();

    let contents = read_save_file(&path).await.unwrap().expect("has <END>");
    let line = contents.error_message.as_ref().expect("the `!` line");
    assert_eq!(line.boot_count(), 1);
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

/// The boot restore's count is `(int)atol(&bp[1])` on the whole line
/// (dbrestore.c:999): a sign is read, an indented line counts 0. The
/// counts are the C oracle IOC's `reboot_restore` lines for this file;
/// past `LONG_MAX` the 64-bit row is the oracle's, the 32-bit one derived.
#[epics_macros_rs::epics_test]
async fn a_boot_restore_counts_the_line_as_c_atol() {
    use epics_base_rs::runtime::log::ErrlogCapture;
    use epics_base_rs::server::autosave::save_set::{RestorePass, restore_boot_file};
    use epics_base_rs::server::autosave::settings::SrSettings;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.sav");
    epics_base_rs::runtime::fs::write(
        &path,
        "# save/restore V5.1\tgenerated\n\
         ! -3 channel(s) neg\n\
         !+4 plus\n  \
         ! 7 indented\n\
         !7 nospace\n\
         ! 99999999999999999999 big\n\
         ! 0x10 hex\n\
         !x none\n\
         \t!5 tab\n\
         <END>\n",
    )
    .await
    .unwrap();
    let errlog = ErrlogCapture::start();

    let db = PvDatabase::new();
    restore_boot_file(&db, &path, RestorePass::One, "", &SrSettings::default())
        .await
        .unwrap();
    let heard = errlog.finish();

    let big = if std::ffi::c_long::BITS == 64 {
        "-1"
    } else {
        "2147483647"
    };
    let counted: Vec<String> = heard
        .iter()
        .map(|l| l.trim_end().to_string())
        .filter(|l| l.ends_with("had no saved value."))
        .collect();
    let want: Vec<String> = ["-3", "4", "0", "7", big, "0", "0", "0"]
        .iter()
        .map(|n| format!("../dbrestore.c:reboot_restore {n} PVs had no saved value."))
        .collect();
    assert_eq!(counted, want);
}

/// `fdbrestore` counts the line as `atoi` of the text after its first word
/// (save_restore.c:3322, :3427), and returns the count as its status. The
/// counts are the C oracle IOC's for each line; the 32-bit rows derived.
#[epics_macros_rs::epics_test]
async fn a_manual_restore_counts_the_line_as_c_atoi() {
    use epics_base_rs::server::autosave::manager::AutosaveBuilder;
    use epics_base_rs::server::autosave::save_set::{ManualSource, do_manual_restore};
    use epics_base_rs::server::autosave::settings::SrSettings;

    let settings = SrSettings::default();
    settings.set_incomplete_sets_ok(1);
    let mgr = AutosaveBuilder::new()
        .settings(std::sync::Arc::new(settings))
        .build()
        .await;
    let lp64 = std::ffi::c_long::BITS == 64;
    let cases = [
        ("! -3 x", -3),
        ("!+4", 0),
        ("! +5 x", 5),
        ("!7 x", 0),
        ("!  8", 8),
        ("! 99999999999 x", if lp64 { 1215752191 } else { i32::MAX }),
        ("! 99999999999999999999 x", if lp64 { -1 } else { i32::MAX }),
        ("!\t9", 9),
        ("  ! 10", 10),
        ("! 0x11", 0),
        ("!", 0),
        ("! ", 0),
        ("!\u{0b}12", 12),
    ];
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.sav");
    let db = PvDatabase::new();
    let mut counted = Vec::new();
    for (line, _) in cases {
        epics_base_rs::runtime::fs::write(&path, format!("# h\n{line}\n<END>\n"))
            .await
            .unwrap();
        let status = do_manual_restore(&db, &mgr, "m.sav", &path, ManualSource::SaveFile, "").await;
        counted.push((line, status));
    }
    assert_eq!(counted, cases);
}
