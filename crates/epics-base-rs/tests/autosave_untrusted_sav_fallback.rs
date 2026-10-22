//! The restore fallbacks C `fopen_and_check` (dbrestore.c:1236-1311)
//! takes from a `.sav` with no `<END>`: `.savB`, then the newest sequence
//! file, at boot and for `fdbrestore`. rs never leaves such a `.sav` after
//! a failed write, as C's in-place `write_it` does, so these are reached
//! from a file torn some other way. The expected lines, values and files
//! are the C oracle IOC's.

use std::path::Path;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOSAVE_UNTRUSTED_SAV_FALLBACK_SCRIPT";

/// The script, in the child only.
#[epics_macros_rs::epics_test]
async fn child_boot() {
    let Some(script) = std::env::var_os(CHILD) else {
        return;
    };
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(script.to_str().unwrap())
        .run(|_| async move {
            epics_base_rs::runtime::log::errlog_flush();
            Ok(())
        })
        .await
        .unwrap();
}

/// The child's `dbrestore.c` errlog lines with the save directory written
/// `DIR`, `A` as the script's closing `dbpr` shows it, and the files left
/// in the save directory, in byte order.
fn run(d: &Path, seq_files: usize, tail: &str) -> (Vec<String>, f64, Vec<String>) {
    std::fs::write(d.join("a.db"), "record(ao, \"A\") { }\n").unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_savefile_path(\"{d}/sav\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         save_restoreSet_NumSeqFiles({seq_files})\n\
         {tail}\
         dbpr A\n",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_boot", "--nocapture", "--test-threads=1"])
        .env(CHILD, d.join("st.cmd"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    // The script's own spelling, which C extends with `/`.
    let sav = format!("{}/sav", d.display());
    let errlog = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.contains("dbrestore.c"))
        .map(|l| l.replace(&sav, "DIR"))
        .collect();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let a = stdout
        .lines()
        .find_map(|l| l.split_once("VAL : ").map(|(_, v)| v.trim()))
        .unwrap_or_else(|| panic!("{stdout}"))
        .parse()
        .unwrap();
    let mut files: Vec<String> = std::fs::read_dir(d.join("sav"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    (errlog, a, files)
}

fn sav_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("sav")).unwrap();
    for (name, text) in files {
        std::fs::write(dir.path().join("sav").join(name), text).unwrap();
    }
    dir
}

fn lines(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|l| l.to_string()).collect()
}

#[test]
fn a_boot_restore_of_an_untrusted_sav_reads_the_savb() {
    let dir = sav_dir(&[
        ("s.sav", "# V5.0\nA 5\n"),
        ("s.savB", "# V5.0\nA 6\n<END>\n"),
    ]);
    let got = run(dir.path(), 0, "set_pass1_restoreFile(\"s.sav\")\niocInit\n");
    assert_eq!(
        got,
        (
            lines(&[
                "../dbrestore.c:checkFile File 'DIR/s.sav' is not trusted.",
                "../dbrestore.c:fopen_and_check Trying backup file 'DIR/s.savB'",
            ]),
            6.0,
            lines(&["s.sav", "s.sav.bu", "s.savB", "s.sav_RBAD_"]),
        )
    );
}

#[test]
fn a_boot_restore_of_an_untrusted_sav_and_savb_reads_the_newest_seq_file() {
    let dir = sav_dir(&[
        ("s.sav", "# V5.0\nA 5\n"),
        ("s.savB", "# V5.0\nA 6\n"),
        ("s.sav0", "# V5.0\nA 7\n<END>\n"),
        ("s.sav1", "# V5.0\nA 8\n<END>\n"),
    ]);
    let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(dir.path().join("sav/s.sav0"))
        .unwrap()
        .set_modified(hour_ago)
        .unwrap();
    let got = run(dir.path(), 2, "set_pass1_restoreFile(\"s.sav\")\niocInit\n");
    assert_eq!(
        got,
        (
            lines(&[
                "../dbrestore.c:checkFile File 'DIR/s.sav' is not trusted.",
                "../dbrestore.c:fopen_and_check Trying backup file 'DIR/s.savB'",
                "../dbrestore.c:checkFile File 'DIR/s.savB' is not trusted.",
                "../dbrestore.c:fopen_and_check Trying backup file 'DIR/s.sav1'",
            ]),
            8.0,
            lines(&[
                "s.sav",
                "s.sav.bu",
                "s.sav0",
                "s.sav1",
                "s.savB",
                "s.savB_RBAD_",
                "s.sav_RBAD_",
            ]),
        )
    );
}

#[test]
fn fdbrestore_of_an_untrusted_file_reads_its_savb() {
    let dir = sav_dir(&[
        ("f.sav", "# V5.0\nA 5\n"),
        ("f.savB", "# V5.0\nA 6\n<END>\n"),
        ("s.req", "A\n"),
    ]);
    let got = run(
        dir.path(),
        0,
        // The oracle finds `s.req` in its working directory.
        &format!(
            "set_requestfile_path(\"{}/sav\")\n\
             iocInit\ncreate_manual_set(\"s.req\")\nepicsThreadSleep(1)\nfdbrestore(\"f.sav\")\nepicsThreadSleep(2)\n",
            dir.path().display()
        ),
    );
    assert_eq!(
        got,
        (
            lines(&[
                "../dbrestore.c:checkFile File 'DIR/f.sav' is not trusted.",
                "../dbrestore.c:fopen_and_check Trying backup file 'DIR/f.savB'",
            ]),
            6.0,
            lines(&["f.sav", "f.sav.bu", "f.savB", "f.sav_RBAD_", "s.req"]),
        )
    );
}
