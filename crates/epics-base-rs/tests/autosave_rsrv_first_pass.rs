//! A save task started before iocInit has a CA context rsrv serves
//! (save_restore.c:802, dbCa.c:344). C `connect_list` gives each channel a
//! connection callback, so its `ca_pend_io` waits for none and a channel
//! made while rsrv serves is still searching at the check and through that
//! pass's `get_channel_values` (save_restore.c:1294-1376). The callback
//! sets `channel_connected` afterwards, so a later `connect_list` searches
//! none of them again (:731-745, :1290). The expected lines are the C
//! oracle IOC's for the same scripts.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
};

mod autosave_wait;

const CHILD: &str = "AUTOSAVE_RSRV_FIRST_PASS_DIR";

/// `wait_not_connected req n`: hold the script until the list made from
/// `req` has fetched and left at most `n` members unconnected.
fn wait_not_connected(holder: Arc<Mutex<AutosaveStartupConfig>>) -> CommandDef {
    CommandDef::new(
        "wait_not_connected",
        vec![
            ArgDesc {
                name: "req",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "n",
                arg_type: ArgType::Int,
            },
        ],
        "hold the script until a list has at most n members not connected",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let (Some(ArgValue::String(req)), Some(&ArgValue::Int(n))) =
                (args.first(), args.get(1))
            else {
                return Err("wait_not_connected req n".to_string());
            };
            let req = req.to_str()?.to_string();
            let mgr = holder.lock().unwrap().manager.clone().unwrap();
            ctx.block_on(async {
                for _ in 0..6000 {
                    if let Some((set, _)) = mgr.find_set_by_request_file(&req)
                        && (0..=n).contains(&i64::from(set.not_connected()))
                    {
                        return Ok(CommandOutcome::Continue);
                    }
                    epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
                }
                Err(format!("wait_not_connected: {req} not fetched"))
            })
        },
    )
}

#[epics_macros_rs::epics_test]
async fn child_ioc() {
    let Some(dir) = std::env::var_os(CHILD).map(std::path::PathBuf::from) else {
        return;
    };
    let holder = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
    IocApplication::new()
        .port(0)
        .autosave_startup(holder.clone())
        .register_startup_command(autosave_wait::wait_ops_served(holder.clone()))
        .register_startup_command(wait_not_connected(holder))
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

/// Runs `body` after the common head and returns stdout.
fn run(d: &Path, body: &str) -> String {
    std::fs::create_dir_all(d.join("sav")).unwrap();
    std::fs::write(
        d.join("a.db"),
        "record(ao, \"A\") { field(VAL, \"3\") }\n\
         record(waveform, \"W\") { field(FTVL, \"DOUBLE\") field(NELM, \"4\") }\n\
         record(stringout, \"S\") { field(VAL, \"abc\") }\n\
         record(bo, \"P:DONE\") {}\n",
    )
    .unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}\")\n\
         set_savefile_path(\"{d}/sav\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         {body}\
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `show`'s channel lines from `marker` on, the chid left out.
fn channel_lines(stdout: &str, marker: &str) -> Vec<String> {
    stdout
        .lines()
        .skip_while(|l| *l != marker)
        .skip(1)
        .take_while(|l| !l.starts_with("reboot-restore status:"))
        .filter(|l| l.contains(" chid:") || l.contains(" not connected (or ca_get failed)"))
        .map(|l| match l.split_once(" chid:") {
            Some((name, rest)) => {
                format!("{name} {}", rest.split_once(' ').map_or("", |(_, r)| r))
            }
            None => l.to_string(),
        })
        .collect()
}

#[test]
fn a_list_made_after_ioc_init_finds_its_rsrv_channels_searching() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("j.req"), "S\n").unwrap();
    std::fs::write(d.join("k.req"), "A\nW\n").unwrap();
    let stdout = run(
        d,
        "create_manual_set(\"j.req\")\n\
         iocInit\n\
         wait_ops_served\n\
         echo ==k\n\
         save_restoreSet_Debug(2)\n\
         create_manual_set(\"k.req\")\n\
         wait_not_connected k.req 2\n\
         wait_ops_served\n\
         save_restoreSet_Debug(0)\n\
         echo ==show1\n\
         save_restoreShow(1)\n\
         manual_save(\"k.req\")\n\
         wait_ops_served\n\
         echo ==show2\n\
         save_restoreShow(1)\n",
    );
    let pass: Vec<&str> = stdout
        .lines()
        .skip_while(|l| *l != "==k")
        .take_while(|l| *l != "==show1")
        .filter(|l| {
            l.starts_with("save_restore: connect failed")
                || l.contains("PV's connected")
                || l.starts_with("save_restore:get_channel_values:")
        })
        .collect();
    assert_eq!(
        pass,
        [
            "save_restore: connect failed for channel 'A'",
            "save_restore: connect failed for channel 'W'",
            "k.sav: 0 of 2 PV's connected",
            "save_restore:get_channel_values: A not connected",
            "save_restore:get_channel_values: W not connected",
            "save_restore:get_channel_values: invalid channel A",
            "save_restore:get_channel_values: invalid channel W",
        ],
        "{stdout}"
    );
    let j = [
        "    1 channel  not connected (or ca_get failed)",
        "\tS state:Conn (max:0 curr:0 elements)\tSearch Issued   channel_connected = 1",
    ];
    assert_eq!(
        channel_lines(&stdout, "==show1"),
        [
            "    2 channels not connected (or ca_get failed)",
            "\tA state:Conn (max:0 curr:0 elements)\tSearch Issued   channel_connected = 1",
            "\tW state:Conn (max:0 curr:0 elements)\tSearch Issued   channel_connected = 1",
        ]
        .iter()
        .chain(&j)
        .map(|l| l.to_string())
        .collect::<Vec<_>>(),
        "{stdout}"
    );
    assert_eq!(
        channel_lines(&stdout, "==show2"),
        [
            "    0 channels not connected (or ca_get failed)",
            "\tA state:Conn (max:1 curr:1 elements)\t3   channel_connected = 1",
            "\tW state:Conn (max:4 curr:0 elements)\t0   channel_connected = 1",
        ]
        .iter()
        .chain(&j)
        .map(|l| l.to_string())
        .collect::<Vec<_>>(),
        "{stdout}"
    );
    let sav = std::fs::read_to_string(d.join("sav/k.sav")).unwrap();
    let body: Vec<&str> = sav.lines().skip(1).collect();
    assert_eq!(body, ["A 3", "W 0", "<END>"], "{sav}");
}

#[test]
fn a_channel_made_before_ioc_init_connects_through_its_callback() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("t.req"), "A\n").unwrap();
    // A trigger that never connects leaves the list unenabled, so each
    // pass runs `connect_list(plist, 1)` again.
    let stdout = run(
        d,
        "create_triggered_set(\"t.req\", \"NOPE\")\n\
         iocInit\n\
         wait_not_connected t.req 0\n\
         wait_ops_served\n\
         echo ==show\n\
         save_restoreShow(1)\n",
    );
    let counts: Vec<&str> = stdout
        .lines()
        .take_while(|l| *l != "==show")
        .filter(|l| l.starts_with("t.sav: "))
        .collect();
    assert_eq!(
        counts.first(),
        Some(&"t.sav: 0 of 1 PV's connected"),
        "{stdout}"
    );
    assert_eq!(
        counts.last(),
        Some(&"t.sav: 0 of 0 PV's connected"),
        "{stdout}"
    );
    assert!(
        counts.iter().all(|l| l.starts_with("t.sav: 0 of ")),
        "{stdout}"
    );
    assert_eq!(
        channel_lines(&stdout, "==show"),
        [
            "    0 channels not connected (or ca_get failed)",
            "\tA state:Conn (max:1 curr:1 elements)\t3   channel_connected = 1",
        ],
        "{stdout}"
    );
}
