//! C routes a saved value by its `@array@` marker (dbrestore.c:914,
//! save_restore.c:3334), so an array value on a one-element field is
//! restored as an array: the boot restore's `SR_array_restore` writes its
//! first element through `dbPutConvertRoutine` (`:355`), which takes no
//! `DBF_MENU` and writes nothing for no elements, and refuses a `DBF_DEVICE`
//! or link field (`:472`); the manual restore's `ca_array_put` writes it as
//! the channel's CA type. A plain value on a one-element waveform is
//! `scalar_restore`'s `dbPut` of the text (`:296-302`). The expected lines
//! are the C oracle IOC's for the same records, file and script.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOSAVE_ARRAY_ON_ONE_ELEMENT_SCRIPT";

const DB: &str = r#"
record(ao, "B") { field(VAL, "3") }
record(ao, "C") { field(VAL, "3") }
record(ao, "D") { field(VAL, "3") }
record(ao, "F") { field(VAL, "3") }
record(ao, "A") { field(VAL, "3") }
record(stringin, "S") { field(VAL, "init") }
record(mbbo, "M") { field(VAL, "1") }
record(longout, "L") { field(VAL, "9") }
record(waveform, "N1") { field(FTVL, "DOUBLE") field(NELM, "1") }
record(waveform, "N2") { field(FTVL, "DOUBLE") field(NELM, "1") }
record(waveform, "N3") { field(FTVL, "DOUBLE") field(NELM, "1") }
record(waveform, "NS") { field(FTVL, "STRING") field(NELM, "1") }
record(waveform, "N4") { field(FTVL, "DOUBLE") field(NELM, "1") }
"#;

const SAV: &str = "# save/restore V5.x\tAutomatically generated - DO NOT MODIFY - 261020-120000\n\
B @array@ { \"5\" }\n\
C @array@ { }\n\
D @array@ { \"x\" }\n\
F @array@ { \"1\" \"2\" }\n\
A.SCAN @array@ { \"1\" }\n\
A.FLNK @array@ { \"B\" }\n\
A.DTYP @array@ { \"1\" }\n\
A.DESC @array@ { \"hello\" }\n\
S @array@ { \"a\" \"b\" }\n\
M @array@ { \"2\" }\n\
L @array@ { \"3.9\" }\n\
N1 5\n\
N2 @array@ { \"7\" }\n\
N3 5abc\n\
NS @array@ { \"q\" }\n\
N4 @array@ { }\n\
<END>\n";

/// Each field after the boot restore and after the manual one.
const FIELDS: [&str; 16] = [
    "B", "C", "D", "F", "A.SCAN", "A.FLNK", "A.DTYP", "A.DESC", "S", "M", "L", "N1", "N2", "N3",
    "NS", "N4",
];

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

/// The child's stdout and stderr for the script `script` makes of the
/// directory holding `a.db`, `x.req` and `r.sav`.
fn run(script: impl FnOnce(&std::path::Path) -> String) -> (String, String) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("a.db"), DB).unwrap();
    std::fs::write(d.join("x.req"), "N1\n").unwrap();
    std::fs::write(d.join("r.sav"), SAV).unwrap();
    std::fs::write(d.join("st.cmd"), script(d)).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_script", "--nocapture", "--test-threads=1"])
        .env(CHILD, d.join("st.cmd"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The child's markers and `dbgf`/`dbpf` lines on stdout, and its autosave
/// errlog lines on stderr.
fn console() -> (Vec<String>, Vec<String>) {
    let dbgf = |fields: &mut dyn Iterator<Item = &&str>| {
        fields
            .map(|f| format!("dbgf {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let fields = dbgf(&mut FIELDS.iter());
    let (out, err) = run(|d| {
        format!(
            "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}\")\n\
         set_savefile_path(\"{d}\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         set_pass1_restoreFile(\"r.sav\")\n\
         iocInit\n\
         echo ==boot\n\
         {fields}\n\
         create_manual_set(\"x.req\")\n\
         epicsThreadSleep 1\n\
         dbpf B 3\n\
         dbpf C 3\n\
         dbpf D 3\n\
         dbpf F 3\n\
         dbpf A.SCAN 0\n\
         dbpf A.FLNK \"\"\n\
         dbpf A.DESC \"\"\n\
         dbpf S init\n\
         dbpf M 1\n\
         dbpf L 9\n\
         dbpf N1 9\n\
         dbpf N2 9\n\
         dbpf N3 9\n\
         dbpf NS z\n\
         dbpf N4 6\n\
         echo ==manual\n\
         fdbrestore(\"r.sav\")\n\
         epicsThreadSleep 2\n\
         {fields}\n\
         echo ==end\n",
            d = d.display()
        )
    });
    let stdout = out
        .lines()
        .map(|l| l.strip_prefix("test child_script ... ").unwrap_or(l))
        .filter(|l| l.starts_with("==") || l.starts_with("DBF_"))
        .map(|l| l.trim_end().to_string())
        .collect();
    let stderr = err
        .lines()
        .filter(|l| l.contains("dbrestore.c:") || l.contains("save_restore.c:"))
        .map(str::to_string)
        .collect();
    (stdout, stderr)
}

#[test]
fn an_array_value_on_a_one_element_field_is_restored_as_an_array() {
    let (stdout, stderr) = console();
    let boot = [
        "DBF_DOUBLE:         5",
        "DBF_DOUBLE:         3",
        "DBF_DOUBLE:         0",
        "DBF_DOUBLE:         1",
        "DBF_STRING:         \"Passive\"",
        "DBF_STRING:         \"\"",
        "DBF_STRING:         \"Soft Channel\"",
        "DBF_STRING:         \"hello\"",
        "DBF_STRING:         \"a\"",
        "DBF_USHORT:         2 = 0x2",
        "DBF_LONG:           3 = 0x3",
        "DBF_DOUBLE:         5",
        "DBF_DOUBLE:         7",
        "DBF_DOUBLE:         5",
        "DBF_STRING:         \"q\"",
        "DBF_DOUBLE[0]: (empty)",
    ];
    let dbpf = [
        "DBF_DOUBLE:         3",
        "DBF_DOUBLE:         3",
        "DBF_DOUBLE:         3",
        "DBF_DOUBLE:         3",
        "DBF_STRING:         \"Passive\"",
        "DBF_STRING:         \"\"",
        "DBF_STRING:         \"\"",
        "DBF_STRING:         \"init\"",
        "DBF_USHORT:         1 = 0x1",
        "DBF_LONG:           9 = 0x9",
        "DBF_DOUBLE:         9",
        "DBF_DOUBLE:         9",
        "DBF_DOUBLE:         9",
        "DBF_STRING:         \"z\"",
        "DBF_DOUBLE:         6",
    ];
    let mut manual = boot.to_vec();
    manual[4] = "DBF_STRING:         \"Event\"";
    manual[5] = "DBF_STRING:         \"B\"";
    manual[6] = "DBF_STRING:         \"Raw Soft Channel\"";
    let mut expected = vec!["==boot"];
    expected.extend(boot);
    expected.extend(dbpf);
    expected.push("==manual");
    expected.extend(manual);
    expected.push("==end");
    assert_eq!(stdout, expected, "{stderr:?}");
    assert_eq!(
        stderr,
        [
            "../dbrestore.c:SR_put_array_values PV A.SCAN: bad field type '12'",
            "../dbrestore.c:reboot_restore restore for PV 'A.SCAN' failed",
            "../dbrestore.c:SR_array_restore field_type 'DBF_FWDLINK' not handled",
            "../dbrestore.c:reboot_restore restore for PV 'A.FLNK' failed",
            "../dbrestore.c:SR_array_restore field_type 'DBF_DEVICE' not handled",
            "../dbrestore.c:reboot_restore restore for PV 'A.DTYP' failed",
        ]
    );
}

/// `num_read` counts only the elements the type switch stores
/// (dbrestore.c:543, :624-645), so a refused field's trace stays at 0.
#[test]
fn a_refused_field_reads_no_elements() {
    let (_, err) = run(|d| {
        format!(
            "dbLoadRecords(\"{d}/a.db\")\n\
             set_savefile_path(\"{d}\")\n\
             save_restoreSet_DatedBackupFiles(0)\n\
             set_pass1_restoreFile(\"r.sav\")\n\
             save_restoreSet_Debug(10)\n\
             iocInit\n",
            d = d.display()
        )
    });
    let dtyp: Vec<&str> = err
        .lines()
        .skip_while(|l| !l.ends_with("PV = 'A.DTYP'"))
        .take_while(|l| !l.contains("looking for ARRAY_END"))
        .filter(|l| l.contains("element["))
        .collect();
    assert_eq!(
        dtyp,
        [
            "../dbrestore.c:SR_array_restore looking for element[0] ",
            "../dbrestore.c:SR_array_restore element[0] value = '1'",
            "../dbrestore.c:SR_array_restore looking for element[0] ",
        ]
    );
}
