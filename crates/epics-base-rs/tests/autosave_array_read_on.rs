//! C reads an `@array@` value on through the file's following lines to its
//! `}` (`SR_array_restore`, dbrestore.c:477-602; `manual_array_restore`,
//! save_restore.c:2984-3090): the lines it reads are array data, not PVs,
//! an element is quoted and at most 39 characters, the rest of the line
//! after the `}` is dropped, and the end of the file before the `}` writes
//! nothing. The expected lines are the C oracle IOC's for the same records,
//! files and script.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOSAVE_ARRAY_READ_ON_SCRIPT";

const DB: &str = r#"
record(waveform, "W") { field(FTVL, "DOUBLE") field(NELM, "4") }
record(waveform, "X") { field(FTVL, "DOUBLE") field(NELM, "4") }
record(waveform, "Y") { field(FTVL, "DOUBLE") field(NELM, "4") }
record(waveform, "Z") { field(FTVL, "STRING") field(NELM, "4") }
record(ao, "B") {}
record(ao, "C") {}
record(ao, "D") {}
record(ao, "E") {}
"#;

const HEADER: &str =
    "# save/restore V5.x\tAutomatically generated - DO NOT MODIFY - 261020-120000\n";

const T_SAV: &str = "W @array@ { \"1\" \"2\"\n\
B 7\n\
\"3\" }  C 8\n\
C 9\n\
#X @array@ { \"4\"\n\
D 5\n\
}\n\
X @array@ { 1 2 }\n\
Y @array@ { \"6\" } junk\n\
Z @array@ { \"abcdefghijabcdefghijabcdefghijabcdefghij-cut\" \"x\\\"y\" }\n\
E 6\n\
<END>\n";

const U_SAV: &str = "E 2\nW @array@ { \"1\"\nB 7\n<END>\n";

/// The st.cmd, in the child only.
#[epics_macros_rs::epics_test]
async fn child_script() {
    let Some(script) = std::env::var_os(CHILD) else {
        return;
    };
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(script.to_str().unwrap())
        .run(|config| async move {
            if let Some(mgr) = config.autosave_manager {
                mgr.shutdown();
            }
            Ok(())
        })
        .await
        .unwrap();
}

/// The oracle's script with `boot` as the pass-1 file: each record after the
/// boot restore and, after `t.sav`, after a manual restore of each file.
fn script(dir: &std::path::Path, boot: &str) -> String {
    let d = dir.display();
    let dbgf =
        |records: &str| -> Vec<String> { records.chars().map(|r| format!("dbgf {r}")).collect() };
    let mut lines = vec![
        format!("dbLoadRecords(\"{d}/a.db\")"),
        format!("set_requestfile_path(\"{d}\")"),
        format!("set_savefile_path(\"{d}\")"),
        "save_restoreSet_DatedBackupFiles(0)".into(),
        format!("set_pass1_restoreFile(\"{boot}\")"),
        "iocInit".into(),
        "echo ==boot".into(),
    ];
    lines.extend(dbgf("WXYZBCDE"));
    if boot == "t.sav" {
        lines.extend([
            "create_manual_set(\"x.req\")".into(),
            "epicsThreadSleep 1".into(),
        ]);
        lines.extend("WXYZ".chars().map(|r| format!("dbpf {r} 9")));
        lines.extend("BCDE".chars().map(|r| format!("dbpf {r} 0")));
        lines.extend([
            "echo ==manual".into(),
            "fdbrestore(\"t.sav\")".into(),
            "epicsThreadSleep 2".into(),
        ]);
        lines.extend(dbgf("WXYZBCDE"));
        lines.extend([
            "dbpf W 9".into(),
            "dbpf B 0".into(),
            "echo ==manualeof".into(),
            "fdbrestore(\"u.sav\")".into(),
            "epicsThreadSleep 2".into(),
        ]);
        lines.extend(dbgf("WBE"));
    }
    lines.push("echo ==end".into());
    lines.join("\n") + "\n"
}

/// The child's markers, `dbgf`/`dbpf` and autosave lines on stdout, and its
/// autosave errlog lines on stderr.
fn console(boot: &str) -> (Vec<String>, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("a.db"), DB).unwrap();
    std::fs::write(d.join("x.req"), "X\n").unwrap();
    std::fs::write(d.join("t.sav"), format!("{HEADER}{T_SAV}")).unwrap();
    std::fs::write(d.join("u.sav"), format!("{HEADER}{U_SAV}")).unwrap();
    std::fs::write(d.join("st.cmd"), script(d, boot)).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_script", "--nocapture", "--test-threads=1"])
        .env(CHILD, d.join("st.cmd"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.strip_prefix("test child_script ... ").unwrap_or(l))
        .filter(|l| l.starts_with("==") || l.starts_with("DBF_") || l.starts_with("save_restore:"))
        .map(|l| l.trim_end().to_string())
        .collect();
    let stderr = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.contains("dbrestore.c:") || l.contains("save_restore.c:"))
        .map(str::to_string)
        .collect();
    (stdout, stderr)
}

const T_VALUES: [&str; 8] = [
    "DBF_DOUBLE[3]:      1         2         3",
    "DBF_DOUBLE[0]: (empty)",
    "DBF_DOUBLE:         6",
    "DBF_STRING[2]:      \"abcdefghijabcdefghijabcdefghijabcdefghi\"         \"x\\\"y\"",
    "DBF_DOUBLE:         0",
    "DBF_DOUBLE:         9",
    "DBF_DOUBLE:         0",
    "DBF_DOUBLE:         6",
];

#[test]
fn an_array_reads_on_through_the_lines_after_it() {
    let (stdout, stderr) = console("t.sav");
    let mut expected = vec!["==boot"];
    expected.extend(T_VALUES);
    // The `dbpf`s.
    expected.extend(["DBF_DOUBLE:         9"; 3]);
    expected.push("DBF_STRING:         \"9\"");
    expected.extend(["DBF_DOUBLE:         0"; 4]);
    expected.extend(["==manual", "save_restore:fdbrestore:entry"]);
    expected.extend(T_VALUES);
    expected.extend([
        "DBF_DOUBLE:         9",
        "DBF_DOUBLE:         0",
        "==manualeof",
        "save_restore:fdbrestore:entry",
        "save_restore:manual_array_restore: status = end_of_file.",
        "save_restore:do_manual_restore: manual_array_restore() returned 1",
        "DBF_DOUBLE:         9",
        "DBF_DOUBLE:         0",
        "DBF_DOUBLE:         2",
        "==end",
    ]);
    assert_eq!(stdout, expected, "{stderr:?}");
    assert_eq!(
        stderr,
        ["../save_restore.c:manual_array_restore *** EOF during array-parse"]
    );
}

#[test]
fn the_end_of_the_file_inside_an_array_writes_nothing() {
    let (stdout, stderr) = console("u.sav");
    let mut expected = vec!["==boot"];
    expected.extend(["DBF_DOUBLE[0]: (empty)"; 3]);
    expected.push("DBF_STRING[0]: (empty)");
    expected.extend(["DBF_DOUBLE:         0"; 3]);
    expected.extend(["DBF_DOUBLE:         2", "==end"]);
    assert_eq!(stdout, expected, "{stderr:?}");
    assert_eq!(
        stderr,
        [
            "../dbrestore.c:SR_array_restore *** EOF during array-parse",
            "../dbrestore.c:SR_array_restore status = end_of_file.",
            "../dbrestore.c:reboot_restore restore for PV 'W.VAL' failed",
        ]
    );
}
