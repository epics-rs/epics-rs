//! C's `lockList`/`unlockList` traces at `save_restoreDebug` 15
//! (save_restore.c:514-530), each beside the line C prints next to it. The
//! expected lines are the C oracle IOC's for this st.cmd.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOSAVE_LIST_LOCK_TRACE_DIR";

const LOCK: &str = "lockList: listLock=1";
const UNLOCK: &str = "unlockList: listLock=0";

#[epics_macros_rs::epics_test]
async fn child_ioc() {
    let Some(dir) = std::env::var_os(CHILD).map(std::path::PathBuf::from) else {
        return;
    };
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(dir.join("st.cmd").to_str().unwrap())
        // The script's lines after iocInit run beside this; returning ends
        // the IOC, so it waits for the script's last put.
        .run(|config| async move {
            epics_base_rs::server::db_server::announce_serving("probe");
            for _ in 0..200 {
                if config
                    .db
                    .get_pv("P:DONE")
                    .is_ok_and(|v| v.to_string() == "1")
                {
                    break;
                }
                epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
            }
            Ok(())
        })
        .await
        .unwrap();
}

fn stdout() -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(
        d.join("a.db"),
        "record(bo, \"P:SR_heartbeat\") {}\nrecord(bo, \"P:DONE\") {}\n",
    )
    .unwrap();
    std::fs::write(d.join("s.req"), "P:SR_heartbeat\n").unwrap();
    std::fs::write(d.join("u.sav"), "# x\nA 2\n<END>\n").unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}\")\n\
         set_savefile_path(\"{d}\")\n\
         save_restoreSet_status_prefix(\"P:\")\n\
         save_restoreSet_Debug(15)\n\
         create_manual_set(\"s.req\")\n\
         save_restoreSet_Debug(0)\n\
         iocInit\n\
         epicsThreadSleep 2.5\n\
         echo ==pass\n\
         save_restoreSet_Debug(15)\n\
         epicsThreadSleep 1\n\
         save_restoreSet_Debug(0)\n\
         echo ==save\n\
         save_restoreSet_Debug(15)\n\
         manual_save(\"s.req\")\n\
         epicsThreadSleep 0.2\n\
         echo ==name\n\
         create_manual_set(\"s.req\")\n\
         set_savefile_name(\"s.req\",\"t.sav\")\n\
         set_savefile_name(\"x.req\",\"t.sav\")\n\
         echo ==show\n\
         save_restoreShow(0)\n\
         echo ==ops\n\
         fdbrestore(\"u.sav\")\n\
         epicsThreadSleep 0.5\n\
         remove_data_set(\"x.req\")\n\
         epicsThreadSleep 0.5\n\
         remove_data_set(\"s.req\")\n\
         epicsThreadSleep 0.5\n\
         save_restoreSet_Debug(0)\n\
         echo ==end\n\
         dbpf P:DONE 1\n",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_ioc", "--nocapture", "--test-threads=1"])
        .env(CHILD, d)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.strip_prefix("test child_ioc ... ").unwrap_or(l))
        // The shell's own echo of each line is bold.
        .filter(|l| !l.starts_with('\u{1b}'))
        .map(str::to_string)
        .collect()
}

/// Does `lines` hold `window` as consecutive lines?
fn holds(lines: &[String], window: &[&str]) -> bool {
    lines
        .windows(window.len())
        .any(|w| w.iter().zip(window).all(|(l, want)| l == want))
}

/// The lines from `from` up to the next `==` marker.
fn section<'a>(lines: &'a [String], from: &str) -> &'a [String] {
    let at = lines.iter().position(|l| l == from).unwrap() + 1;
    let len = lines[at..]
        .iter()
        .position(|l| l.starts_with("=="))
        .unwrap();
    &lines[at..at + len]
}

#[test]
fn each_walk_of_the_lists_is_traced_as_c() {
    let lines = stdout();
    let create = "save_restore:create_data_set: file 's.req', method 10, period 0, trig_chan 'NONE', mon_period 0";
    for window in [
        // create_data_set: the lookup, then the link of the new list.
        &[create, LOCK, UNLOCK][..],
        &[
            "save_restore:readReqFile: exit: reqFile='s.req'.",
            LOCK,
            UNLOCK,
        ],
        &[
            "    0 channels not connected (or ca_get failed)",
            UNLOCK,
            "reboot-restore status:",
        ],
        &["save_restore task: manual save('')", LOCK],
        &[UNLOCK, "save_restore: manual save status=-1 (0==success)"],
        &[
            "save_restore:do_manual_restore: entry for file 'u.sav'",
            LOCK,
            UNLOCK,
        ],
        &[
            "save_restore task: calling do_remove_data_set('x.req')",
            LOCK,
            UNLOCK,
            "save_restore: remove status=-1 (0==success)",
        ],
        &[
            "save_restore task: calling do_remove_data_set('s.req')",
            LOCK,
            UNLOCK,
            LOCK,
            UNLOCK,
            "save_restore: remove status=0 (0==success)",
        ],
    ] {
        assert!(holds(&lines, window), "{window:?} not in {lines:#?}");
    }
    // save_restoreShow walks the lists after the save file path's line.
    let path = lines.iter().position(|l| l == "  save file path:").unwrap();
    assert_eq!(lines[path + 2], LOCK, "{lines:#?}");
    // A pass walks the lists, then the lists' status PVs.
    let pass = section(&lines, "==pass");
    assert!(!pass.is_empty(), "{lines:#?}");
    assert!(
        pass.chunks(4).all(|c| c == [LOCK, UNLOCK, LOCK, UNLOCK]),
        "{pass:#?}"
    );
    // A create refused for a method the list has (its line is errlog's,
    // between the two), then set_savefile_name, found and not. The op just
    // served holds the next pass off for a second.
    assert_eq!(
        section(&lines, "==name"),
        [create, LOCK, UNLOCK, LOCK, UNLOCK, LOCK, UNLOCK]
    );
}
