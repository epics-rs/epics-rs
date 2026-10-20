// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — its one hand-built tokio runtime ran in a suite that passed under `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p epics-base-rs --test autosave_as_verify` (7/7).
//! C `asVerify(filename, verbose, restoreFileName)` (save_restore.c:2877)
//! queues `do_asVerify` (verify.c:41-452) to the save task: the file named,
//! under the save path, compared PV by PV with the live values on stdout,
//! and with a restore file named, the live values written to
//! `<restoreFileName>B` and copied over `restoreFileName` when fewer than
//! half the PVs were not connected.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

const CHILD: &str = "AUTOSAVE_AS_VERIFY_DIR";
const LINE: &str = "AUTOSAVE_AS_VERIFY_LINE";
/// Set: the child prints [`RETURNED`] once the shell line has returned.
const TASK: &str = "AUTOSAVE_AS_VERIFY_TASK";
/// What the child prints once the shell line has returned.
const RETURNED: &str = "-- shell line returned --";
/// Set: the child has no save task, so C's `opMsgQueue` is NULL.
const NO_TASK: &str = "AUTOSAVE_AS_VERIFY_NO_TASK";

const SAV: &str =
    "# h\nA 1.5\nB 3\nA.DESC d\nA.PREC 2\nA.SCAN 0\nX 1\n#C.VAL Search Issued\n<END>\n";

/// The child's stdout for the shell line `line` run on `dir`, with `dir`
/// written `DIR` and the test harness's own lines dropped.
fn output_of(dir: &Path, line: &str) -> Vec<String> {
    run_child(dir, line, None)
}

/// [`output_of`] with the child's `flag` set.
fn run_child(dir: &Path, line: &str, flag: Option<&str>) -> Vec<String> {
    let name = "child_as_verify";
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD, dir)
        .env(LINE, line.replace("DIR", &dir.display().to_string()));
    if let Some(flag) = flag {
        cmd.env(flag, "1");
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let prefix = format!("test {name} ... ");
    let dir = dir.display().to_string();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.strip_prefix(prefix.as_str()).unwrap_or(l))
        .filter(|l| {
            !(l.is_empty()
                || *l == "ok"
                || l.starts_with("running ")
                || l.starts_with("test result"))
        })
        .map(|l| l.replace(&dir, "DIR"))
        .collect()
}

/// `$LINE` run in an iocsh with the autosave startup commands and a running
/// save task, the save path `$CHILD`, `A` = 1.5 with DESC "d" and `B` = 2,
/// in the child only.
#[test]
fn child_as_verify() {
    let Some(dir) = std::env::var_os(CHILD).map(PathBuf::from) else {
        return;
    };
    let line = std::env::var(LINE).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let db = Arc::new(PvDatabase::new());
    rt.block_on(async {
        db.add_record("A", Box::new(AoRecord::new(1.5)))
            .await
            .unwrap();
        db.add_record("B", Box::new(AoRecord::new(2.0)))
            .await
            .unwrap();
        db.put_pv("A.DESC", EpicsValue::String("d".into()))
            .await
            .unwrap();
    });
    let holder = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
    // A built manager has C's task and queue, so NO_TASK attaches none.
    let task = std::env::var_os(NO_TASK).is_none().then(|| {
        let mgr = Arc::new(rt.block_on(AutosaveBuilder::new().build()));
        holder.lock().unwrap().manager = Some(mgr.clone());
        let handle = mgr.clone().start(db.clone());
        (mgr, handle)
    });
    holder.lock().unwrap().save_file_path = Some(dir);
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    std::thread::spawn(move || {
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        let shell = IocShell::new(db, bridge);
        for cmd in AutosaveStartupConfig::register_startup_commands(holder) {
            shell.register(cmd);
        }
        for line in line.split(';') {
            shell.execute_line(line).unwrap();
        }
        if std::env::var_os(TASK).is_some() {
            println!("{RETURNED}");
        }
    })
    .join()
    .unwrap();
    if let Some((mgr, _)) = task {
        rt.block_on(mgr.ops_served());
    }
}

/// With a save task, asVerify is its op (save_restore.c:2874, :1239-1248).
/// C orders the task's lines against neither the shell's return nor its
/// prompt, so the child waits for the task before they are read.
#[test]
fn the_save_task_lists_what_differed_at_verbose_0() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.sav"), SAV).unwrap();
    assert_eq!(
        output_of(dir.path(), "asVerify(\"s.sav\", 0, \"\")"),
        [
            "    PVname saved_value live_value",
            "    =============================",
            "*** B                         3.000000                  2.000000",
            "*** A.PREC                   '2'                       '0'",
            "asVerify: *** 'X' not connected.  Saved value='1'",
            "2 PVs differed.  (5 PVs checked; 2 PVs not connected)",
        ]
    );
}

/// With no save task C's `opMsgQueue` is NULL and asVerify's send crashes
/// (:2874), a defect not copied: the op is dropped, and the shell does not
/// verify in the task's place.
#[test]
fn without_a_save_task_nothing_is_verified() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.sav"), SAV).unwrap();
    let lines = run_child(dir.path(), "asVerify(\"s.sav\", 0, \"\")", Some(NO_TASK));
    assert!(lines.is_empty(), "{lines:?}");
}

/// Every op is the task's, in the order sent (save_restore.c:1131): the
/// verify sent first sees B before the restore sent after it.
#[test]
fn a_verify_sent_before_a_restore_is_served_first() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("v.sav"), "# h\nB 3\n<END>\n").unwrap();
    for _ in 0..10 {
        let lines = run_child(
            dir.path(),
            "asVerify(\"v.sav\", 0, \"\");fdbrestore(\"v.sav\")",
            Some(TASK),
        );
        assert_eq!(
            lines.iter().rev().find(|l| l.contains("differed")).unwrap(),
            "1 PV differed.  (1 PV checked; 0 PVs not connected)",
            "{lines:?}"
        );
    }
}

/// verbose 1: every PV's line, the not-connected one in file order, and the
/// summary (:446-449). A `#` line naming "Search Issued" is one more PV not
/// connected (:128).
#[test]
fn every_pv_is_listed_at_verbose_1() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.sav"), SAV).unwrap();
    assert_eq!(
        output_of(dir.path(), "asVerify(\"s.sav\", 1, \"\")"),
        [
            "    PVname saved_value live_value",
            "    =============================",
            "    A                         1.500000                  1.500000",
            "*** B                         3.000000                  2.000000",
            "    A.DESC                   'd'                       'd'",
            "*** A.PREC                   '2'                       '0'",
            "    A.SCAN                    0                         0",
            "asVerify: *** 'X' not connected.  Saved value='1'",
            "2 PVs differed.  (5 PVs checked; 2 PVs not connected)",
        ]
    );
}

/// verbose -1 says nothing a PV that agrees has to say, nor the summary; the
/// restore file holds the live values and is not copied over
/// `restoreFileName`, since 2 PVs not connected is not fewer than 5 / 2
/// (:434).
#[test]
fn a_restore_file_is_the_live_values() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.sav"), SAV).unwrap();
    assert_eq!(
        output_of(dir.path(), "asVerify(\"s.sav\", -1, \"DIR/r.sav\")"),
        [
            "    PVname saved_value live_value",
            "    =============================",
            "*** B                         3.000000                  2.000000",
            "*** A.PREC                   '2'                       '0'",
            "asVerify: *** 'X' not connected.  Saved value='1'",
        ]
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("r.savB")).unwrap(),
        "# asVerify R5.3\tAutomatically generated - DO NOT MODIFY - datetime\n\
         # h\nA 1.5\nB 2\nA.DESC d\nA.PREC 0\nA.SCAN 0\n#'X' not connected\n\
         #C.VAL Search Issued\n<END>\n"
    );
    assert!(!dir.path().join("r.sav").exists());
}

/// One PV not connected, four checked: 1 < 4 / 2, so the restore file is
/// copied over.
#[test]
fn a_restore_file_is_copied_when_most_pvs_connected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("s.sav"),
        "A 1.5\nB 3\nA.SCAN 0\nB.SCAN 0\nX 1\n<END>\n",
    )
    .unwrap();
    output_of(dir.path(), "asVerify(\"s.sav\", 0, \"DIR/r.sav\")");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("r.sav")).unwrap(),
        std::fs::read_to_string(dir.path().join("r.savB")).unwrap()
    );
}

/// :85-94, and the file still compared.
#[test]
fn a_file_without_its_end_marker_is_named_and_compared() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("t.sav"), "A 1.5\n").unwrap();
    assert_eq!(
        output_of(dir.path(), "asVerify(\"t.sav\", 1, \"\")"),
        [
            "asVerify: Can't find <END> marker.  File may be bad.",
            "    PVname saved_value live_value",
            "    =============================",
            "    A                         1.500000                  1.500000",
            "0 PVs differed.  (1 PV checked; 0 PVs not connected)",
        ]
    );
}

/// An enum's saved value is C `atoi`'d (verify.c:298, :309): glibc's
/// saturating `strtol` cut to `int`, so twenty nines read -1 where `long` is
/// 64 bits.
#[test]
fn an_enum_value_is_read_as_c_atoi_reads_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("e.sav"),
        "A 1.5\nA.SCAN 99999999999999999999\n<END>\n",
    )
    .unwrap();
    assert_eq!(
        output_of(dir.path(), "asVerify(\"e.sav\", 1, \"\")"),
        [
            "    PVname saved_value live_value".to_string(),
            "    =============================".to_string(),
            "    A                         1.500000                  1.500000".to_string(),
            format!(
                "*** {:<25} {:<25} 0",
                "A.SCAN",
                epics_base_rs::types::c_parse::long_to_int(std::ffi::c_long::MAX)
            ),
            "1 PV differed.  (2 PVs checked; 0 PVs not connected)".to_string(),
        ]
    );
}

/// :44-47.
#[test]
fn a_file_that_cannot_be_opened_is_named() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        output_of(dir.path(), "asVerify(\"none.sav\", 1, \"\")"),
        ["asVerify: Can't open 'DIR/none.sav'."]
    );
}

/// The arrays' `(array) N diffs` line (:224-227), over the elements CA
/// returns: an array in the file is compared as one, even on a scalar.
#[test]
fn an_array_is_counted_by_element() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("s.sav"),
        "A @array@ { \"1.5\" }\nB @array@ { \"2\" \"7\" }\n<END>\n",
    )
    .unwrap();
    assert_eq!(
        output_of(dir.path(), "asVerify(\"s.sav\", 1, \"\")"),
        [
            "    PVname saved_value live_value",
            "    =============================",
            "    A                         (array) 0 diffs",
            "    B                         (array) 0 diffs",
            "0 PVs differed.  (2 PVs checked; 0 PVs not connected)",
        ]
    );
}
