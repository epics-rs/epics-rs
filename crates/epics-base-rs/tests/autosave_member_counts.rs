//! C keeps each member's element counts from `connect_list`
//! (save_restore.c:1320-1360): `ca_element_count` of its channel, 0 for one
//! `dbChannelCreate` refused, then for an array `SR_get_array_info`'s count
//! of the name without its `$`. The expected lines are the C oracle IOC's
//! for the same script.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOSAVE_MEMBER_COUNTS_DIR";

#[epics_macros_rs::epics_test]
async fn child_ioc() {
    let Some(dir) = std::env::var_os(CHILD).map(std::path::PathBuf::from) else {
        return;
    };
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(dir.join("st.cmd").to_str().unwrap())
        // Returning ends the IOC, so it waits for the script's last put.
        .run(|config| async move {
            epics_base_rs::server::db_server::announce_serving("probe");
            let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
            while std::time::Instant::now() < deadline {
                if config.db.get_pv("P:DONE").is_ok_and(|v| v.text_g() == "1") {
                    break;
                }
                epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
            }
            Ok(())
        })
        .await
        .unwrap();
}

/// The child's stdout for a manual set of `W`, `A.DESC$`, a member whose
/// filter C does not know, and a filtered `W`.
fn run() -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(
        d.join("a.db"),
        "record(ao, \"A\") { field(DESC, \"hello\") }\n\
         record(waveform, \"W\") { field(FTVL, \"DOUBLE\") field(NELM, \"4\") }\n\
         record(bo, \"P:DONE\") {}\n",
    )
    .unwrap();
    std::fs::write(
        d.join("s.req"),
        "W\nA.DESC$\nA.VAL{\"nofilter\":{}}\nW.VAL{\"arr\":{\"s\":0,\"e\":1}}\n",
    )
    .unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}\")\n\
         set_savefile_path(\"{d}\")\n\
         iocInit\n\
         dbpf W \"[1,2,3]\"\n\
         save_restoreSet_Debug(10)\n\
         create_manual_set(\"s.req\")\n\
         epicsThreadSleep(3)\n\
         save_restoreSet_Debug(0)\n\
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

#[test]
fn connect_list_counts_each_members_elements_as_c() {
    let lines = run();
    let counts: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .filter(|l| l.contains("has, at most") || l.contains("is not local"))
        .collect();
    assert_eq!(
        counts,
        [
            "save_restore:connect_list: 'W' has, at most, 4 elements",
            "save_restore:connect_list:(after SR_get_array_info) 'W' has, at most, 4 elements",
            "save_restore:connect_list: 'A.DESC$' has, at most, 41 elements",
            "save_restore:connect_list:(after SR_get_array_info) 'A.DESC$' has, at most, 1 elements",
            "save_restore:connect_list: 'A.VAL{\"nofilter\":{}}' has, at most, 0 elements",
            "save_restore:connect_list: 'W.VAL{\"arr\":{\"s\":0,\"e\":1}}' has, at most, 4 elements",
            "save_restore:connect_list:(after SR_get_array_info) 'W.VAL{\"arr\":{\"s\":0,\"e\":1}}' has, at most, 4 elements",
        ],
        "{lines:#?}"
    );
}

/// `save_restoreShow`'s member lines, each chid address written PTR: each count is
/// the one `connect_list` and `get_channel_values` kept, and a `$` member's
/// value is its first char read as `DBR_STRING`.
#[test]
fn the_show_prints_the_counts_c_kept() {
    let lines = run();
    let members: Vec<String> = lines
        .iter()
        .map(String::as_str)
        .filter(|l| l.starts_with('\t') && l.contains("state:Conn"))
        .map(mask_chid)
        .collect();
    assert_eq!(
        members,
        [
            "\tW chid:PTR state:Conn (max:4 curr:3 elements)\t1   channel_connected = 1",
            "\tA.DESC$ chid:PTR state:Conn (max:1 curr:1 elements)\t104   channel_connected = 1",
            "\tW.VAL{\"arr\":{\"s\":0,\"e\":1}} chid:PTR state:Conn (max:4 curr:3 elements)\t1   channel_connected = 1",
        ],
        "{lines:#?}"
    );
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
