//! A boot restore of a field C's static `dbPutString` writes and rs does not
//! let a put write: the C oracle IOC, restoring `A.RTYP x` and `A.PACT 1`
//! on an ao in either pass, prints nothing and reads RTYP "x" and PACT 1. rs
//! keeps the field (a C defect, not copied) and prints nothing too.
//! `A.NAME x` is left out: C renames the record, and each later line then
//! fails its `dbFindRecord`.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOSAVE_SILENT_FIELD_RESTORE_DIR";

/// Boot from `$CHILD/st.cmd`, then print each field's value as `F=value`.
#[epics_macros_rs::epics_test]
async fn child_ioc() {
    let Some(dir) = std::env::var_os(CHILD).map(std::path::PathBuf::from) else {
        return;
    };
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(dir.join("st.cmd").to_str().unwrap())
        .run(|config| async move {
            epics_base_rs::server::db_server::announce_serving("probe");
            for field in FIELDS {
                let value = config.db.get_pv(format!("A.{}", field.0)).unwrap();
                println!("{}={value}", field.0);
            }
            Ok(())
        })
        .await
        .unwrap();
}

/// Each field, its saved value, and the value it keeps.
const FIELDS: [(&str, &str, &str); 6] = [
    ("RTYP", "x", "ao"),
    ("PACT", "1", "0"),
    ("PUTF", "1", "0"),
    ("ORAW", "1", "0"),
    ("RBV", "1", "0"),
    // A field rs writes is written.
    ("DESC", "d", "d"),
];

/// The console's restore lines and the fields' values after a boot restore
/// in `pass`.
fn boot(pass: u8) -> (Vec<String>, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("a.db"), "record(ao, \"A\") { field(VAL, \"3\") }\n").unwrap();
    let lines: String = FIELDS
        .iter()
        .map(|(f, v, _)| format!("A.{f} {v}\n"))
        .collect();
    std::fs::write(d.join("s.sav"), format!("# h\n{lines}<END>\n")).unwrap();
    std::fs::write(
        d.join("st.cmd"),
        format!(
            "dbLoadRecords(\"{d}/a.db\")\n\
             set_savefile_path(\"{d}\")\n\
             save_restoreSet_DatedBackupFiles(0)\n\
             set_pass{pass}_restoreFile(\"s.sav\")\n\
             iocInit\n",
            d = d.display()
        ),
    )
    .unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_ioc", "--nocapture", "--test-threads=1"])
        .env(CHILD, d)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = |bytes: &[u8]| -> Vec<String> {
        String::from_utf8_lossy(bytes)
            .lines()
            .map(|l| l.strip_prefix("test child_ioc ... ").unwrap_or(l))
            .map(str::to_string)
            .collect()
    };
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    let console = stdout
        .iter()
        .chain(&stderr)
        // The shell's own echo of each line is bold.
        .filter(|l| !l.starts_with('\u{1b}'))
        .filter(|l| l.contains("restore") || l.contains("filename="))
        .cloned()
        .collect();
    let values = stdout
        .into_iter()
        .filter(|l| FIELDS.iter().any(|(f, ..)| l.starts_with(&format!("{f}="))))
        .collect();
    (console, values)
}

#[test]
fn a_field_c_overwrites_is_kept_without_a_line() {
    let kept: Vec<String> = FIELDS.iter().map(|(f, _, v)| format!("{f}={v}")).collect();
    for pass in [0, 1] {
        let (console, values) = boot(pass);
        assert_eq!(console, Vec::<String>::new(), "pass {pass}");
        assert_eq!(values, kept, "pass {pass}");
    }
}
