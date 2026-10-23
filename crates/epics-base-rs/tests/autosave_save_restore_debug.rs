//! save_restore.c's `save_restoreDebug` traces: each line at its C site,
//! text and threshold. The expected lines are the C oracle IOC's for the
//! same script: a monitor set with an include, then `manual_save`,
//! `fdbrestore` and `remove_data_set` after `iocInit`.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOSAVE_SR_DEBUG_SCRIPT";

const DB: &str = r#"
record(ao, "A") {}
record(waveform, "W") { field(FTVL, "DOUBLE") field(NELM, "4") }
record(ao, "B") {}
"#;

/// The C oracle's script, then the save task iocInit started for a few
/// passes, in the child only.
#[epics_macros_rs::epics_test]
async fn child_debug() {
    let Some(script) = std::env::var_os(CHILD) else {
        return;
    };
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(script.to_str().unwrap())
        .run(|config| async move {
            let mgr = config.autosave_manager.unwrap();
            // The script's lines after iocInit run, and queue their ops.
            epics_base_rs::server::db_server::announce_serving("probe");
            sleep(Duration::from_secs(3)).await;
            mgr.shutdown();
            Ok(())
        })
        .await
        .unwrap();
}

/// The child's stdout lines that start `save_restore` or `manual_save`.
fn traces(level: i32) -> Vec<String> {
    stdout_lines(level)
        .into_iter()
        .filter(|l| l.starts_with("save_restore") || l.starts_with("manual_save"))
        .collect()
}

/// The child's stdout, `dir` written `DIR` and a pointer written `ADDR`.
fn stdout_lines(level: i32) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir_all(d.join("req")).unwrap();
    std::fs::create_dir_all(d.join("sav")).unwrap();
    std::fs::write(d.join("a.db"), DB).unwrap();
    std::fs::write(d.join("req/s.req"), "A\nfile inc.req P=$(P)\n").unwrap();
    std::fs::write(d.join("req/inc.req"), "$(P)\nW\nNOPE\n").unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}/req\")\n\
         set_savefile_path(\"{d}/sav\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         save_restoreSet_Debug({level})\n\
         create_monitor_set(\"s.req\", 5, \"P=B\")\n\
         iocInit\n\
         manual_save(\"s.req\")\n\
         fdbrestore(\"s.sav\")\n\
         create_monitor_set(\"s.req\", 5, \"P=B\")\n\
         remove_data_set(\"s.req\")\n\
         epicsThreadSleep(1.5)\n\
         create_monitor_set(\"s.req\", 5, \"P=B\")\n",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_debug", "--nocapture", "--test-threads=1"])
        .env(CHILD, d.join("st.cmd"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let clean = |l: &str, dir: &Path| {
        let mut l = l.replace(&dir.display().to_string(), "DIR");
        while let Some(at) = l.find("0x") {
            let end = l[at + 2..]
                .find(|c: char| !c.is_ascii_hexdigit())
                .map_or(l.len(), |e| at + 2 + e);
            l.replace_range(at..end, "ADDR");
        }
        l
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| clean(l, d))
        .collect()
}

fn has(lines: &[String], line: &str) -> bool {
    lines.iter().any(|l| l == line)
}

#[test]
fn debug_2_traces_the_request_file_and_the_ops() {
    let lines = traces(2);
    for line in [
        "save_restore:create_data_set: file 's.req', method c, period 0, trig_chan 'NONE', mon_period 5",
        "save_restore:readReqFile: entry: reqFile='s.req', plist=ADDR, macrostring='P=B'",
        "save_restore:readReqFile: preparing to include file: eline='file inc.req P=B",
        "save_restore:readReqFile: calling readReqFile('inc.req', ADDR,'P=B')",
        "save_restore:readReqFile: exit: reqFile='s.req'.",
        "save_restore:save_restore: entry; status_prefix=''",
        "save_restore: exiting",
        "manual_save: request_file='s.req', save_file='(null)', callbackFunction=(nil), puserPvt=(nil)",
        "save_restore task: manual save('')",
        "save_restore:get_channel_values: NOPE not connected",
        "save_restore:get_channel_values: invalid channel NOPE",
        "save_restore task: calling do_manual_restore('s.sav')",
        "save_restore: manual restore status=1 (0==success)",
        "save_restore task: calling do_remove_data_set('s.req')",
        "save_restore: remove status=0 (0==success)",
    ] {
        assert!(has(&lines, line), "{line}: {lines:#?}");
    }
    assert!(
        !lines.iter().any(|l| l.contains("enable_list")),
        "{lines:#?}"
    );
}

#[test]
fn debug_5_adds_the_list_and_restore_traces() {
    let lines = traces(5);
    for line in [
        "save_restore:readReqFile: Current macro definitions:",
        "save_restore:enable_list: entry",
        "save_restore:enable_list: done calling ca_add_event for list channels",
        "save_restore:concatenate_paths: dest='DIR/sav/s.sav'",
        "save_restore:request_manual_restore: entry",
        "save_restore:do_manual_restore: entry for file 's.sav'",
    ] {
        assert!(has(&lines, line), "{line}: {lines:#?}");
    }
    assert!(
        !lines.iter().any(|l| l.contains("connect_list: channel")),
        "{lines:#?}"
    );
    // `s.sav` is the list's last save file, so C restores the fetched
    // values and reads no file.
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("do_manual_restore: PVname=")),
        "{lines:#?}"
    );
}

/// C's task prints `save_restoreShow(1)` before its last line when Debug is
/// set (save_restore.c:1263-1266); the shutdown flag is already up, so no
/// member is listed.
#[test]
fn the_task_shows_its_lists_as_it_exits() {
    let lines = stdout_lines(2);
    let begin = lines.iter().rposition(|l| l == "BEGIN save_restoreShow");
    let end = lines.iter().rposition(|l| l == "END save_restoreShow");
    let (Some(begin), Some(end)) = (begin, end) else {
        panic!("{lines:#?}");
    };
    assert_eq!(lines[end + 1], "save_restore: exiting", "{lines:#?}");
    let show = &lines[begin..end];
    assert!(show.contains(&"  Debug level: 2".to_string()), "{show:#?}");
    assert!(show.contains(&"  s.req: ".to_string()), "{show:#?}");
    assert!(!show.iter().any(|l| l.starts_with('\t')), "{show:#?}");
}

#[test]
fn debug_0_exits_without_the_show() {
    let lines = stdout_lines(0);
    assert!(
        !lines.iter().any(|l| l.contains("save_restoreShow")),
        "{lines:#?}"
    );
}
