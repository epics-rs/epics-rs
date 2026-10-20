//! C's `save_restoreShow` member lines print the list's `struct channel`
//! as `connect_list` and `get_channel_values` left it
//! (save_restore.c:2564-2570): its chid, "noChid" for none, and the kept
//! counts and value. The expected lines are the C oracle IOC's for the same
//! script, with a two-second sleep for each `wait_ops_served`, each chid
//! address written as PTR.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

mod autosave_wait;

const CHILD: &str = "AUTOSAVE_SHOW_CHANNELS_DIR";

#[epics_macros_rs::epics_test]
async fn child_ioc() {
    let Some(dir) = std::env::var_os(CHILD).map(std::path::PathBuf::from) else {
        return;
    };
    let holder = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
    IocApplication::new()
        .port(0)
        .autosave_startup(holder.clone())
        .register_startup_command(autosave_wait::wait_ops_served(holder))
        .startup_script(dir.join("st.cmd").to_str().unwrap())
        // Returning ends the IOC, so it waits for the script's last put.
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

/// The child's stdout for a manual set of `A`, a PV no record serves and a
/// member whose filter C does not know, shown before `iocInit`, after it,
/// and after a save. Nothing gates the task's first `connect_list` on the
/// script (save_restore.c:935-956), so the first show waits for that pass.
fn run() -> Vec<String> {
    run_of("A\nNOSUCH\nA.VAL{\"nofilter\":{}}\n")
}

/// [`run`] of a set whose request file is `req`.
fn run_of(req: &str) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(
        d.join("a.db"),
        "record(ao, \"A\") { field(VAL, \"3\") }\nrecord(bo, \"P:DONE\") {}\n",
    )
    .unwrap();
    std::fs::write(d.join("p.req"), req).unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}\")\n\
         set_savefile_path(\"{d}\")\n\
         create_manual_set(\"p.req\")\n\
         wait_ops_served\n\
         echo ==pre\n\
         save_restoreShow(1)\n\
         iocInit\n\
         epicsThreadSleep(3)\n\
         echo ==post\n\
         save_restoreShow(1)\n\
         dbpf A 5\n\
         manual_save(\"p.req\")\n\
         wait_ops_served\n\
         dbpf A 6\n\
         echo ==saved\n\
         save_restoreShow(1)\n\
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
        .map(str::to_string)
        .collect()
}

/// The member lines after `marker`, each `chid:0x...` written `chid:PTR`.
fn members(lines: &[String], marker: &str) -> Vec<String> {
    lines
        .iter()
        .skip_while(|l| *l != marker)
        .skip(1)
        .take_while(|l| !l.starts_with("=="))
        .filter(|l| l.starts_with('\t'))
        .map(|l| mask_chid(l))
        .collect()
}

/// `line` with its `chid:0x...` address written `chid:PTR`.
fn mask_chid(line: &str) -> String {
    match line.find("chid:0x") {
        Some(i) => {
            let end = line[i + 5..].find(' ').map_or(line.len(), |e| i + 5 + e);
            format!("{}chid:PTR{}", &line[..i], &line[end..])
        }
        None => line.to_string(),
    }
}

#[test]
fn the_show_prints_each_members_channel_as_c_kept_it() {
    let lines = run();
    // Before `iocInit` the task's CA context is not the database's, so its
    // first pass made every channel and none connected.
    assert_eq!(
        members(&lines, "==pre"),
        [
            "\tA chid:PTR state:Never (max:0 curr:0 elements)\tSearch Issued   channel_connected = 0",
            "\tNOSUCH chid:PTR state:Never (max:0 curr:0 elements)\tSearch Issued   channel_connected = 0",
            "\tA.VAL{\"nofilter\":{}} chid:PTR state:Never (max:0 curr:0 elements)\tSearch Issued   channel_connected = 0",
        ],
        "{lines:#?}"
    );
    // A connected after the pass that made its channel, so the pass kept
    // no count and C still prints "Search Issued".
    assert_eq!(
        members(&lines, "==post"),
        [
            "\tA chid:PTR state:Conn (max:0 curr:0 elements)\tSearch Issued   channel_connected = 1",
            "\tNOSUCH chid:PTR state:Never (max:0 curr:0 elements)\tSearch Issued   channel_connected = 0",
            "\tA.VAL{\"nofilter\":{}} chid:(nil) state:noChid (max:0 curr:0 elements)\tSearch Issued   channel_connected = 0",
        ],
        "{lines:#?}"
    );
    // The save's fetch, 5, not A's 6 since.
    assert_eq!(
        members(&lines, "==saved"),
        [
            "\tA chid:PTR state:Conn (max:1 curr:1 elements)\t5   channel_connected = 1",
            "\tNOSUCH chid:PTR state:Never (max:0 curr:0 elements)\tSearch Issued   channel_connected = 0",
            "\tA.VAL{\"nofilter\":{}} chid:(nil) state:noChid (max:0 curr:0 elements)\tSearch Issued   channel_connected = 0",
        ],
        "{lines:#?}"
    );
}

/// C `connect_list` makes a channel per member (save_restore.c:1286-1373),
/// so a PV listed twice shows two chids, each connected and fetched, as the
/// C oracle IOC does.
#[test]
fn a_pv_listed_twice_has_a_channel_per_member() {
    let lines = run_of("A\nA\n");
    let conn =
        "\tA chid:PTR state:Conn (max:0 curr:0 elements)\tSearch Issued   channel_connected = 1";
    assert_eq!(members(&lines, "==post"), [conn, conn], "{lines:#?}");
    let saved = "\tA chid:PTR state:Conn (max:1 curr:1 elements)\t5   channel_connected = 1";
    assert_eq!(members(&lines, "==saved"), [saved, saved], "{lines:#?}");
    let chids: Vec<&str> = lines
        .iter()
        .skip_while(|l| *l != "==saved")
        .filter_map(|l| l.strip_prefix("\tA chid:"))
        .filter_map(|l| l.split(' ').next())
        .collect();
    assert!(chids.len() == 2 && chids[0] != chids[1], "{chids:?}");
}
