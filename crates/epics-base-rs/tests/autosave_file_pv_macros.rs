//! C reads a list's `SAVEPATHPV`, `SAVENAMEPV` and `CONFIGMENU` through
//! `macGetValue` at the end of each request-file level (save_restore.c:
//! 3689-3699), so each counts only when its expanded value is non-empty and
//! expanded without an undefined name, and an included file's macros count
//! as the top file's do. The expected files, statuses and show lines are
//! the C oracle IOC's for this script.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

mod autosave_wait;

const CHILD: &str = "AUTOSAVE_FILE_PV_MACROS_DIR";

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

/// The files under `dir`, relative and sorted.
fn files(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            found.extend(
                files(&p)
                    .into_iter()
                    .map(|f| format!("{}/{f}", p.file_name().unwrap().to_string_lossy())),
            );
        } else {
            found.push(p.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    found.sort();
    found
}

#[test]
fn each_file_macro_counts_only_with_a_clean_non_empty_value() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir_all(d.join("sav/sub")).unwrap();
    std::fs::write(
        d.join("a.db"),
        "record(ao, \"A\") { field(VAL, \"3\") }\n\
         record(stringout, \"N:name\") { field(VAL, \"named.sav\") }\n\
         record(stringout, \"N:name2\") { field(VAL, \"named2.sav\") }\n\
         record(stringout, \"N:path\") { field(VAL, \"sub\") }\n\
         record(bo, \"P:DONE\") {}\n",
    )
    .unwrap();
    // Each list's macros and request file.
    let sets = [
        ("a", "SAVEPATHPV=$(Q)", "A"),
        ("b", "E=,SAVENAMEPV=$(E)", "A"),
        ("c", "P=N:,SAVENAMEPV=$(P)name", "A"),
        ("d", "E=,CONFIGMENU=$(E)", "A"),
        ("e", "P=N:,SAVEPATHPV=$(P)path,SAVENAMEPV=$(Q)", "A"),
        ("f", "", "file inc.req SAVENAMEPV=N:name2"),
        ("g", "", "file inc.req CONFIGMENU=1"),
        ("h", "P=N:", "file inc.req SAVEPATHPV=$(P)path"),
        ("i", "SAVENAMEPV=$(Q)", "file inc.req SAVENAMEPV=N:name"),
    ];
    std::fs::write(d.join("inc.req"), "A\n").unwrap();
    let mut script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}\")\n\
         set_savefile_path(\"{d}/sav\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         iocInit\n",
        d = d.display()
    );
    for (name, macros, req) in sets {
        std::fs::write(d.join(format!("{name}.req")), format!("{req}\n")).unwrap();
        script += &format!("echo =={name}\ncreate_manual_set(\"{name}.req\", '{macros}')\n");
    }
    script += "wait_ops_served\n";
    for (name, ..) in sets {
        script += &format!("manual_save(\"{name}.req\")\n");
    }
    script += "wait_ops_served\necho ==show\nsave_restoreShow(1)\necho ==end\ndbpf P:DONE 1\n";
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_ioc", "--nocapture", "--test-threads=1"])
        .env(CHILD, d)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // i's manual save copies c's named.sav aside first (:2080-2092).
    let found: Vec<String> = files(&d.join("sav"))
        .into_iter()
        .map(|f| match f.strip_prefix("named.sav_") {
            Some(date) if date.len() == 13 => "named.sav_DATE".into(),
            _ => f,
        })
        .collect();
    assert_eq!(
        found,
        [
            "a.sav",
            "b.sav",
            "d.sav",
            "g.sav",
            "named.sav",
            "named.sav_DATE",
            "named2.sav",
            "sub/e.sav",
            "sub/h.sav"
        ],
        "{stdout}\n{stderr}"
    );
    // A manual save of a list keeping backups has no backup file to make
    // (:2109-2120).
    let statuses: Vec<&str> = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("save_restore:defaultCallback:status="))
        .collect();
    assert_eq!(
        statuses,
        ["-1", "-1", "0", "-1", "0", "0", "0", "0", "0"],
        "{stdout}"
    );
    // Each list's path PV, name PV and backups, in show's order.
    let show: Vec<&str> = stdout
        .lines()
        .skip_while(|l| *l != "==show")
        .map(str::trim)
        .filter(|l| {
            l.ends_with(".req:")
                || l.starts_with("path PV:")
                || l.starts_with("name PV:")
                || l.starts_with("backups:")
        })
        .collect();
    let mut expected = Vec::new();
    for (list, path, name, backups) in [
        ("i", "None", "N:name", "NO"),
        ("h", "N:path", "None", "NO"),
        ("g", "None", "None", "NO"),
        ("f", "None", "N:name2", "NO"),
        ("e", "N:path", "None", "NO"),
        ("d", "None", "None", "YES"),
        ("c", "None", "N:name", "NO"),
        ("b", "None", "None", "YES"),
        ("a", "None", "None", "YES"),
    ] {
        expected.push(format!("{list}.req:"));
        expected.push(format!("path PV: {path}"));
        expected.push(format!("name PV: {name}"));
        expected.push(format!("backups: {backups}"));
    }
    assert_eq!(show, expected, "{stdout}");
    // macLib's notice, once per list whose definitions name Q.
    let notices: Vec<&str> = stderr
        .lines()
        .chain(stdout.lines())
        .filter(|l| l.contains("macLib:"))
        .collect();
    assert_eq!(
        notices,
        [
            "macLib: macro Q is undefined (expanding macro SAVEPATHPV)",
            "macLib: macro Q is undefined (expanding macro SAVENAMEPV)",
            "macLib: macro Q is undefined (expanding macro SAVENAMEPV)",
        ],
        "{stdout}\n{stderr}"
    );
}
