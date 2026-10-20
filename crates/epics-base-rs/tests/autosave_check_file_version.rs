//! C `checkFile` (dbrestore.c:1137-1234) trusts a file whose header has a
//! version below 1.8 without the `<END>` test, and prints
//! `myPrintErrno` for an `fseek` past the start of a short file. The
//! expected lines and values are the C oracle IOC's pass-1 restore of
//! each file.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOSAVE_CHECK_FILE_VERSION_SCRIPT";

/// The boot restore, then `A`'s value printed, in the child only.
#[epics_macros_rs::epics_test]
async fn child_boot() {
    let Some(script) = std::env::var_os(CHILD) else {
        return;
    };
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(script.to_str().unwrap())
        .run(|config| async move {
            println!("A={:?}", config.db.get_pv("A").unwrap().to_f64());
            epics_base_rs::runtime::log::errlog_flush();
            Ok(())
        })
        .await
        .unwrap();
}

/// The child's `dbrestore.c` errlog lines with the directory written
/// `DIR`, and `A` after the restore.
fn boot(contents: &[u8]) -> (Vec<String>, f64, bool) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir_all(d.join("sav")).unwrap();
    std::fs::write(d.join("a.db"), "record(ao, \"A\") { }\n").unwrap();
    std::fs::write(d.join("sav/s.sav"), contents).unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_savefile_path(\"{d}/sav\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         save_restoreSet_NumSeqFiles(0)\n\
         set_pass1_restoreFile(\"s.sav\")\n\
         iocInit\n",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_boot", "--nocapture", "--test-threads=1"])
        .env(CHILD, d.join("st.cmd"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let sav = d.join("sav").display().to_string();
    let errlog = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.contains("dbrestore.c"))
        .map(|l| l.replace(&sav, "DIR"))
        .collect();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let a = stdout
        .lines()
        .find_map(|l| l.split_once("A=Some(").map(|(_, v)| v))
        .and_then(|v| v.strip_suffix(')'))
        .unwrap_or_else(|| panic!("{stdout}"))
        .parse()
        .unwrap();
    (errlog, a, d.join("sav/s.sav_RBAD_").exists())
}

/// C's lines when `s.sav` is refused and nothing else is there.
fn none_left() -> Vec<String> {
    [
        "../dbrestore.c:fopen_and_check Trying backup file 'DIR/s.savB'",
        "../dbrestore.c:checkFile Can't open file 'DIR/s.savB'.",
        "../dbrestore.c:fopen_and_check Can't figure out which seq file is most recent,",
        "../dbrestore.c:fopen_and_check so I'm just going to start with 'DIR/s.sav0'.",
        "../dbrestore.c:fopen_and_check Can't find a file to restore from...",
        "../dbrestore.c:fopen_and_check ...last tried 'DIR/s.sav0'. I give up.",
        "../dbrestore.c:reboot_restore Can't open save file.",
    ]
    .map(String::from)
    .to_vec()
}

/// C trusts a header with no version (:1165-1169), which since autosave
/// 8804347 is the one it writes itself, so the oracle restores a torn file
/// (reviewer3 d17). Not copied: the file needs its `<END>`.
#[test]
fn a_header_with_no_version_needs_its_end_marker() {
    let (errlog, a, rbad) = boot(b"# Autosave release 6.1\tx\nA 5\n");
    let mut expected =
        vec!["../dbrestore.c:checkFile File 'DIR/s.sav' is not trusted.".to_string()];
    expected.extend(none_left());
    assert_eq!((errlog, a, rbad), (expected, 0.0, true));
    let (errlog, a, rbad) = boot(b"# Autosave release 6.1\tx\nA 5\n<END>\n");
    assert_eq!((errlog, a, rbad), (vec![], 5.0, false));
}

#[test]
fn a_version_below_1_8_is_trusted() {
    let (errlog, a, rbad) = boot(b"# V1.7\nA 7\n");
    assert_eq!((errlog, a, rbad), (vec![], 7.0, false));
}

#[test]
fn a_version_1_8_file_needs_its_end_marker() {
    let (errlog, a, rbad) = boot(b"# V1.9\nA 6\n");
    let mut expected =
        vec!["../dbrestore.c:checkFile File 'DIR/s.sav' is not trusted.".to_string()];
    expected.extend(none_left());
    assert_eq!((errlog, a, rbad), (expected, 0.0, true));
}

/// Shorter than the 6-byte seek: its error line, then no word more.
#[test]
fn a_file_too_short_to_seek_is_refused_after_the_errno_line() {
    let (errlog, a, rbad) = boot(b"R5\n");
    let mut expected =
        vec!["../dbrestore.c(1183): [0x16]=checkFile: fseek error :Invalid argument".to_string()];
    expected.extend(none_left());
    assert_eq!((errlog, a, rbad), (expected, 0.0, false));
}

/// Six bytes: the 7-byte seek fails and `fgets` reads on from where the
/// 6-byte one stopped.
#[test]
fn a_six_byte_file_fails_the_second_seek_and_is_not_trusted() {
    let (errlog, a, rbad) = boot(b"R9\nAB\n");
    let mut expected = vec![
        "../dbrestore.c(1202): [0x16]=checkFile: fseek error :Invalid argument".to_string(),
        "../dbrestore.c:checkFile File 'DIR/s.sav' is not trusted.".to_string(),
    ];
    expected.extend(none_left());
    assert_eq!((errlog, a, rbad), (expected, 0.0, true));
}
