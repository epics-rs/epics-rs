// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — its one hand-built tokio runtime ran in a suite that passed under `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p epics-base-rs --test autosave_remove_data_set` (6/6).
//! C `remove_data_set(filename)` (save_restore.c:2648-2728) takes a list
//! out of the save task, freeing its status PV slot; the `reload_*_set`s
//! (:2730-2800, :1170-1199) remove it and make it again.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::records::ao::AoRecord;

const CHILD: &str = "AUTOSAVE_REMOVE_DIR";
const LINE: &str = "AUTOSAVE_REMOVE_LINE";

/// The child's stdout and its errlog lines naming autosave.
fn outputs_of(dir: &Path, line: &str) -> (Vec<String>, Vec<String>) {
    let name = "child_remove";
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD, dir)
        .env(LINE, line)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let prefix = format!("test {name} ... ");
    let stdout = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.strip_prefix(prefix.as_str()).unwrap_or(l).to_owned())
        .collect();
    let errlog = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.contains("save_restore") || l.contains("macro string"))
        .map(str::to_owned)
        .collect();
    (stdout, errlog)
}

/// `$LINE`, `;`-separated, in an iocsh whose running manager, with status
/// prefix `P:`, has the manual set of `s.req` (`A`), saving under
/// `$CHILD`; `t.req` and `u.req` name `A` too. Each line's ops are served
/// before the next line runs.
#[test]
fn child_remove() {
    let Some(dir) = std::env::var_os(CHILD).map(PathBuf::from) else {
        return;
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let db = Arc::new(PvDatabase::new());
    let holder = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
    {
        let mut h = holder.lock().unwrap();
        h.save_file_path = Some(dir.clone());
        h.request_file_paths = vec![dir.clone()];
    }
    let mgr = rt.block_on(async {
        db.add_record("A", Box::new(AoRecord::new(1.5)))
            .await
            .unwrap();
        for req in ["s.req", "t.req", "u.req"] {
            std::fs::write(dir.join(req), "A\n").unwrap();
        }
        let settings = holder.lock().unwrap().settings.clone();
        let mgr = AutosaveBuilder::new()
            .settings(settings)
            .status_prefix("P:")
            .add_set(
                SaveSetConfig {
                    name: "s.req".into(),
                    save_path: dir.join("s.sav"),
                    request_file: Some("s.req".into()),
                    request_pvs: Vec::new(),
                    backup: BackupConfig::default(),
                    macros: Default::default(),
                    macro_string: None,
                    search_paths: vec![dir.clone()],
                },
                SaveStrategy::Manual,
            )
            .build()
            .await;
        let mgr = Arc::new(mgr);
        holder.lock().unwrap().manager = Some(mgr.clone());
        mgr
    });
    let _task = mgr.clone().start(db.clone());
    let handle = rt.handle().clone();
    let line = std::env::var(LINE).unwrap();
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
            handle.block_on(mgr.ops_served());
        }
    })
    .join()
    .unwrap();
    epics_base_rs::runtime::log::errlog_flush();
}

/// The lines of `save_restoreShow`'s listing that name a list, its status
/// PV, its macro string, its methods or its periods.
fn lists(stdout: &[String]) -> Vec<&str> {
    stdout
        .iter()
        .map(String::as_str)
        .filter(|l| {
            l.ends_with(".req: ")
                || l.starts_with("    Status PV:")
                || l.starts_with("    macro string:")
                || l.starts_with("    methods:")
                || l.starts_with("    period:")
        })
        .collect()
}

/// The list is gone: no listing, and a manual save of it finds nothing.
#[test]
fn a_removed_list_is_not_saved() {
    let dir = tempfile::tempdir().unwrap();
    let (out, errlog) = outputs_of(
        dir.path(),
        "remove_data_set(\"s.req\");save_restoreShow(0);manual_save(\"s.req\")",
    );
    assert_eq!(lists(&out), Vec::<&str>::new(), "{out:?}");
    assert!(out.contains(&"save_restore:defaultCallback:status=-1".to_string()));
    assert_eq!(errlog, Vec::<String>::new());
}

/// :2652-2655 and :2718-2720.
#[test]
fn a_bad_or_unknown_filename_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (_, errlog) = outputs_of(
        dir.path(),
        "remove_data_set(\"\");remove_data_set(\"t.req\")",
    );
    assert_eq!(
        errlog,
        [
            "../save_restore.c:remove_data_set bad filename",
            "../save_restore.c:do_remove_data_set Couldn't find 't.req'",
        ]
    );
}

/// C finds the list by `strcmp` on its request file name (:2678), so a
/// spelling of the same path is not that list.
#[test]
fn a_list_is_found_by_its_exact_file_name() {
    let dir = tempfile::tempdir().unwrap();
    let (out, errlog) = outputs_of(
        dir.path(),
        "remove_data_set(\"./s.req\");remove_data_set(\"s.req/\");save_restoreShow(0)",
    );
    assert_eq!(
        errlog,
        [
            "../save_restore.c:do_remove_data_set Couldn't find './s.req'",
            "../save_restore.c:do_remove_data_set Couldn't find 's.req/'",
        ]
    );
    assert!(lists(&out).contains(&"  s.req: "), "{out:?}");
}

/// A new list takes the first free status PV slot (:2466-2470), which a
/// removed one gave back (:2693).
#[test]
fn a_removed_lists_status_pvs_are_reused() {
    let dir = tempfile::tempdir().unwrap();
    let (out, _) = outputs_of(
        dir.path(),
        "create_manual_set(\"t.req\");remove_data_set(\"s.req\");\
         create_manual_set(\"u.req\");save_restoreShow(0)",
    );
    assert_eq!(
        lists(&out),
        [
            "  u.req: ",
            "    macro string: ''",
            "    Status PV: P:SR_0_Status",
            "    methods: [ MANUAL ]",
            "    period: 4; trigger chan: ''; monitor period: 4",
            "  t.req: ",
            "    macro string: ''",
            "    Status PV: P:SR_1_Status",
            "    methods: [ MANUAL ]",
            "    period: 4; trigger chan: ''; monitor period: 4",
        ]
    );
}

/// The list made again with the reload's method and macro string only.
#[test]
fn a_reloaded_list_has_only_the_new_method() {
    let dir = tempfile::tempdir().unwrap();
    let (out, errlog) = outputs_of(
        dir.path(),
        "reload_monitor_set(\"s.req\", 7, \"Q=y\");reload_periodic_set(\"t.req\", 9, \"\");\
         save_restoreShow(0)",
    );
    assert_eq!(
        lists(&out),
        [
            "  s.req: ",
            "    macro string: 'Q=y'",
            "    Status PV: P:SR_0_Status",
            "    methods: [ TIMER+CHANGE ]",
            "    period: 4; trigger chan: ''; monitor period: 7",
        ]
    );
    assert_eq!(
        errlog,
        ["../save_restore.c:do_remove_data_set Couldn't find 't.req'"]
    );
}

/// C removes the list before `create_triggered_set` refuses the channel,
/// so the reload leaves no list (:1178-1186, :2297-2301).
#[test]
fn a_reload_the_create_refuses_leaves_no_list() {
    let dir = tempfile::tempdir().unwrap();
    let (out, errlog) = outputs_of(
        dir.path(),
        "reload_triggered_set(\"s.req\", \"\", \"\");reload_manual_set(\"s.req\", \"\");\
         reload_triggered_set(\"t.req\", \"T\", \"\");save_restoreShow(0)",
    );
    assert_eq!(lists(&out), Vec::<&str>::new(), "{out:?}");
    assert_eq!(
        errlog,
        [
            "save_restore:create_triggered_set: Error: trigger-channel name is required.",
            "../save_restore.c:do_remove_data_set Couldn't find 's.req'",
            "../save_restore.c:do_remove_data_set Couldn't find 't.req'",
        ]
    );
}
