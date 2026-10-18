// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — its one hand-built tokio runtime ran in a suite that passed under `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p epics-base-rs --test autosave_show` (8/8).
//! C `dbrestoreShow()` (dbrestore.c:147-168) lists the boot-restore files
//! with their status; `save_restoreShow(verbose)` (save_restore.c:2492-2583)
//! the settings and the lists before it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::CommandOutcome;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::types::EpicsValue;

const CHILD: &str = "AUTOSAVE_SHOW_DIR";
const LINE: &str = "AUTOSAVE_SHOW_LINE";
/// The manager's set saves under this subdirectory of `$CHILD`.
const SAVE_DIR: &str = "AUTOSAVE_SHOW_SAVE_DIR";
/// Set: the child registers `save_restoreSet_NFSHost`, as an IOC loading
/// `asSupportNfs.dbd` does.
const NFS: &str = "AUTOSAVE_SHOW_NFS";

/// The child's stdout for `line`, `;`-separated, with `dir` written `DIR`
/// and the test harness's own lines dropped.
fn output_of(dir: &Path, line: &str) -> Vec<String> {
    run_child(dir, line, None)
}

/// [`output_of`] with a manager whose set saves under `dir/save_dir`.
fn output_with_set(dir: &Path, line: &str, save_dir: &str) -> Vec<String> {
    run_child(dir, line, Some(save_dir))
}

fn run_child(dir: &Path, line: &str, save_dir: Option<&str>) -> Vec<String> {
    run_child_nfs(dir, line, save_dir, false)
}

fn run_child_nfs(dir: &Path, line: &str, save_dir: Option<&str>, nfs: bool) -> Vec<String> {
    let name = "child_show";
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD, dir)
        .env(LINE, line);
    if nfs {
        cmd.env(NFS, "1");
    }
    if let Some(save_dir) = save_dir {
        cmd.env(SAVE_DIR, save_dir);
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

/// `$LINE` in an iocsh with the autosave startup commands and the save path
/// `$CHILD`, in the child only. With `$SAVE_DIR`, a manager has the manual
/// set of `s.req` (`A` an ao of 1.5, `B` a bo at "zero", `X` no record)
/// with macro string "P=x", saving to `$CHILD/$SAVE_DIR/s.sav`.
#[test]
fn child_show() {
    let Some(dir) = std::env::var_os(CHILD).map(PathBuf::from) else {
        return;
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let db = Arc::new(PvDatabase::new());
    let holder = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
    // The NFS case starts, as C does, with no save path.
    if std::env::var_os(NFS).is_none() {
        holder.lock().unwrap().save_file_path = Some(dir.clone());
    }
    if let Some(save_dir) = std::env::var_os(SAVE_DIR) {
        rt.block_on(async {
            db.add_record("A", Box::new(AoRecord::new(1.5)))
                .await
                .unwrap();
            db.add_record("B", Box::new(BoRecord::new(0)))
                .await
                .unwrap();
            db.put_pv("B.ZNAM", EpicsValue::String("zero".into()))
                .await
                .unwrap();
            std::fs::write(dir.join("s.req"), "A\nB\nX\n").unwrap();
            let settings = holder.lock().unwrap().settings.clone();
            let mgr = AutosaveBuilder::new()
                .settings(settings)
                .add_set(
                    SaveSetConfig {
                        name: "s".into(),
                        save_path: dir.join(save_dir).join("s.sav"),
                        request_file: Some("s.req".into()),
                        request_pvs: Vec::new(),
                        backup: BackupConfig { enable_savb: false },
                        macros: Default::default(),
                        macro_string: "P=x".into(),
                        search_paths: vec![dir.clone()],
                    },
                    SaveStrategy::Manual,
                )
                .build()
                .await;
            holder.lock().unwrap().manager = Some(Arc::new(mgr));
        });
    }
    let line = std::env::var(LINE).unwrap();
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    std::thread::spawn(move || {
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        let shell = IocShell::new(db, bridge);
        if std::env::var_os(NFS).is_some() {
            for cmd in AutosaveStartupConfig::nfs_commands(holder.clone()) {
                shell.register(cmd);
            }
        }
        for cmd in AutosaveStartupConfig::register_startup_commands(holder) {
            shell.register(cmd);
        }
        for line in line.split(';') {
            match shell.execute_line(line) {
                Ok(CommandOutcome::Failed) => println!("FAILED"),
                Ok(_) => {}
                Err(e) => println!("ERR {e}"),
            }
        }
    })
    .join()
    .unwrap();
}

/// Each item with its own status, pass 0's with its macro string or
/// "None".
#[test]
fn dbrestore_show_lists_both_passes() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        output_of(
            dir.path(),
            "set_pass0_restoreFile(\"a.sav\", \"P=x\");set_pass0_restoreFile(\"b.sav\");\
             set_pass1_restoreFile(\"c.sav\", \"Q=y\");dbrestoreShow"
        ),
        [
            "  '     filename     ' -  status  - 'message' - 'macro string'",
            "  pass 0:",
            "  'a.sav' - No Status - 'Unknown, probably failed' - 'P=x'",
            "  'b.sav' - No Status - 'Unknown, probably failed' - 'None'",
            "  pass 1:",
            "  'c.sav' - No Status - 'Unknown, probably failed'",
        ]
    );
}

/// Before `iocInit` there is no task: the settings as the commands left
/// them, each request path ending in '/', and the boot-restore table.
#[test]
fn save_restore_show_before_the_task() {
    let dir = tempfile::tempdir().unwrap();
    let out = output_of(
        dir.path(),
        &format!(
            "set_requestfile_path(\"{}\", \"req\");save_restoreSet_Debug(0);\
             save_restoreSet_NumSeqFiles(12);save_restoreSet_IncompleteSetsOk(0);\
             save_restoreSet_DatedBackupFiles(0);save_restoreSet_RetrySeconds(30);\
             set_pass1_restoreFile(\"c.sav\");save_restoreShow(0)",
            dir.path().display()
        ),
    );
    assert!(
        out[1].starts_with("  Current date-time (yymmdd-hhmmss): [") && out[1].ends_with("] "),
        "{out:?}"
    );
    let mut expected = vec![
        "BEGIN save_restoreShow",
        "  Status: 'No Status' - ''",
        "  Debug level: 0",
    ];
    if cfg!(unix) {
        expected.push("  File permissions: 0664");
    }
    expected.extend([
        "  Save/restore incomplete save sets? NO",
        "  Write dated backup files? NO",
        "  Number of sequence files to maintain: 12",
        "  Time interval between sequence files: 60 seconds",
        "  Time interval between .sav-file write failure and retry: 30 seconds",
        "  I/O errors: 0",
        "  request file path list:",
        "    'DIR/req/'",
        "  save file path:",
        "    'DIR'",
        "  The save_restore task apparently is not running.",
        "reboot-restore status:",
        "  '     filename     ' -  status  - 'message' - 'macro string'",
        "  pass 0:",
        "  pass 1:",
        "  'c.sav' - No Status - 'Unknown, probably failed'",
        "END save_restoreShow",
    ]);
    let mut got = out.clone();
    got.remove(1);
    assert_eq!(got, expected);
}

/// A list as C prints it, a failed `write_it` open counted as an I/O error,
/// and at verbose 1 each member: a menu field as its choice and its index,
/// a member with no record as one never connected.
#[test]
fn save_restore_show_lists_a_set() {
    let dir = tempfile::tempdir().unwrap();
    let out = output_with_set(
        dir.path(),
        "manual_save(\"s.req\");save_restoreShow(1)",
        "none",
    );
    let begin = out
        .iter()
        .position(|l| l == "BEGIN save_restoreShow")
        .unwrap();
    let list = out.iter().position(|l| l == "  s.req: ").unwrap();
    assert!(
        out[begin..].contains(&"  I/O errors: 1".to_string()),
        "{out:?}"
    );
    let lines: Vec<&str> = out[list..].iter().map(String::as_str).collect();
    let time = |l: &str, label: &str| {
        l.strip_prefix(label).is_some_and(|t| {
            chrono::NaiveDateTime::parse_from_str(t, "%a %b %d %H:%M:%S %Y").is_ok()
        })
    };
    assert!(time(lines[4], "    Last save time  :"), "{lines:?}");
    assert!(time(lines[5], "    Last backup time:"), "{lines:?}");
    let mut got = lines.clone();
    got.drain(4..6);
    assert_eq!(
        got[..16],
        [
            "  s.req: ",
            "    macro string: 'P=x'",
            "    Status PV: ",
            "    Status: ' Failure ' - 'Can't write .sav file'",
            "    methods: [ MANUAL ]",
            "    path PV: None",
            "    name PV: None",
            "    backups: NO",
            "    save_state = 0x0",
            "    period: 4; trigger chan: ''; monitor period: 4",
            "    last saved file - ",
            "    0 channels not connected (or ca_get failed)",
            "\tA state:Conn (max:1 curr:1 elements)\t1.5   channel_connected = 1",
            "\tB state:Conn (max:1 curr:1 elements)\tzero   channel_connected = 1\t0",
            "\tX state:Never (max:0 curr:0 elements)\tSearch Issued   channel_connected = 0",
            "reboot-restore status:",
        ]
    );
}

/// A save that landed names its file.
#[test]
fn save_restore_show_names_the_last_saved_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sv")).unwrap();
    let out = output_with_set(
        dir.path(),
        "manual_save(\"s.req\");save_restoreShow(0)",
        "sv",
    );
    assert!(
        out.contains(&"    last saved file - s.sav".to_string()),
        "{out:?}"
    );
    assert!(out.contains(&"  I/O errors: 0".to_string()), "{out:?}");
}

/// The C oracle IOC's lines with `save_restoreNFSRegister` run: the Linux
/// build manages no mount, so each call fails after naming the mount
/// point, a set save path is put under it, and the listing shows the host
/// only while all three settings are set.
#[test]
fn save_restore_set_nfs_host_as_linux_c() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_child_nfs(
        dir.path(),
        "save_restoreSet_NFSHost(\"h\", \"1.2.3.4\", \"\");set_savefile_path(\"/sv\");\
         save_restoreSet_NFSHost(\"h\", \"1.2.3.4\", \"/mnt\");save_restoreShow(0);\
         save_restoreSet_NFSHost(\"h2\", \"5.6.7.8\", \"\");\
         save_restoreSet_NFSHost(\"h3\", \"\", \"/mnt/sv\");save_restoreShow(0)",
        None,
        true,
    );
    let mut got: Vec<&str> = out
        .iter()
        .map(String::as_str)
        .filter(|l| !l.starts_with("  Current date-time"))
        .collect();
    let first_end = got
        .iter()
        .position(|l| *l == "END save_restoreShow")
        .unwrap();
    let second = got.split_off(first_end + 1);
    // Each failure is the line's, as C's `iocshSetError(status)`.
    let mount = [
        "save_restore: Mount point not specified, cannot mount NFS.",
        "FAILED",
        "Autosave is not configured to manage the file-system mount point.",
        "save_restore: Can't mount '/mnt'",
        "FAILED",
    ];
    assert_eq!(got[..5], mount, "{out:?}");
    let nfs_lines = [
        "  NFS host: 'h'; address:'1.2.3.4'",
        "  NFS mount point:",
        "    '/mnt'",
        "  NFS mount status: Ok",
        "  I/O errors: 0",
    ];
    let at = got
        .iter()
        .position(|l| l.starts_with("  NFS host"))
        .unwrap();
    assert_eq!(got[at..at + 5], nfs_lines, "{out:?}");
    assert!(got.contains(&"    '/mnt/sv'"), "{out:?}");
    let twice = [
        "Autosave is not configured to manage the file-system mount point.",
        "Autosave is not configured to manage the file-system mount point.",
        "save_restore: Can't mount '/mnt/sv'",
        "FAILED",
    ];
    assert_eq!(second[..4], twice, "{out:?}");
    assert_eq!(second[4..8], twice, "{out:?}");
    assert!(!second.iter().any(|l| l.starts_with("  NFS")), "{out:?}");
    assert!(second.contains(&"    '/mnt/sv'"), "{out:?}");
}

/// Without `asSupportNfs.dbd`'s registrar the command is unknown.
#[test]
fn save_restore_set_nfs_host_needs_its_registrar() {
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_show", "--nocapture", "--test-threads=1"])
        .env(CHILD, dir.path())
        .env(LINE, "save_restoreSet_NFSHost(\"h\", \"a\", \"/mnt\")")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Command 'save_restoreSet_NFSHost' not registered."),
        "{stderr}"
    );
}

/// The C oracle IOC's listing: a missing `path` or `pathsub` is skipped, a
/// `pathsub` goes under `path` even when absolute, each request path ends
/// in '/', an empty one is not added, and one too long is refused.
#[test]
fn path_commands_join_as_c() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display().to_string();
    let long = "a".repeat(200);
    let line = format!(
        "set_requestfile_path();set_requestfile_path(\"\", \"sub\");\
         set_requestfile_path(\"/a\", \"/b\");set_requestfile_path(\"/c/\");\
         set_requestfile_path(\"/{long}\", \"{long}\");set_savefile_path(\"{d}\", \"/b\");\
         set_savefile_path();set_savefile_path(\"\", \"{d}/x\");save_restoreShow(0)"
    );
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_show", "--nocapture", "--test-threads=1"])
        .env(CHILD, dir.path())
        .env(LINE, &line)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("../save_restore.c:set_requestfile_path 'path'+'pathsub' is too long"),
        "{stderr}"
    );
    let out = output_of(dir.path(), &line);
    let from = out
        .iter()
        .position(|l| l == "  request file path list:")
        .unwrap();
    assert_eq!(
        out[from..from + 7],
        [
            "  request file path list:",
            "    'sub/'",
            "    '/a/b/'",
            "    '/c/'",
            "  save file path:",
            "    'DIR/x'",
            "  The save_restore task apparently is not running.",
        ]
    );
}
