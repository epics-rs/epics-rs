//! dbrestore.c's `save_restoreDebug` traces: each line at its C site, text
//! and threshold. The expected lines are the C oracle IOC's for the same
//! file and records (`save_restoreSet_Debug`, then both boot passes).

use std::path::{Path, PathBuf};

use epics_base_rs::server::autosave::startup::RestoreDef;
use epics_base_rs::server::autosave::{BootRestore, RestorePass, reboot_restore};
use epics_base_rs::server::ioc_builder::IocBuilder;

const CHILD: &str = "AUTOSAVE_DBRESTORE_DEBUG_DIR";
const LEVEL: &str = "AUTOSAVE_DBRESTORE_DEBUG_LEVEL";

const DB: &str = r#"
record(ao, "A") {}
record(waveform, "W") { field(FTVL, "DOUBLE") field(NELM, "4") }
record(ao, "L") { field(OUT, "A") }
"#;

const SAV: &str = "# autosave R5.10\tAutomatically generated - DO NOT MODIFY - 181018-120000\n\
                   A.VAL 1.5\nW.VAL @array@ { \"1\" \"2\" }\nL.OUT $(X) NPP\nL.DESC $(X)\n<END>\n";

/// Both boot passes of `$CHILD/s.sav` with macros `X=B`, the pass-0 one
/// writing the undated boot backup, in the child only.
#[epics_macros_rs::epics_test]
async fn child_debug() {
    let Some(dir) = std::env::var_os(CHILD).map(PathBuf::from) else {
        return;
    };
    let (db, _) = IocBuilder::new()
        .db_string(DB, &Default::default())
        .unwrap()
        .build()
        .await
        .unwrap();
    let status = RestoreDef::new("s.sav".into(), String::new()).status;
    let item = |write_backup| BootRestore {
        filename: "s.sav".into(),
        path: dir.join("s.sav"),
        status: status.clone(),
        settings: Default::default(),
        macrostring: "X=B".into(),
        write_backup,
    };
    let (pass0, pass1) = (item(true), item(false));
    let level = std::env::var(LEVEL).unwrap().parse().unwrap();
    for item in [&pass0, &pass1] {
        item.settings.set_debug(level);
        item.settings.set_dated_backup_files(0);
    }
    println!("BEGIN");
    let _ = reboot_restore(&db, &pass0, RestorePass::Zero).await;
    let _ = reboot_restore(&db, &pass1, RestorePass::One).await;
    println!("END");
    epics_base_rs::runtime::log::errlog_flush();
}

/// The child's stdout between its markers and its dbrestore.c errlog lines,
/// `dir` written `DIR` and a `FILE *` written `ADDR`.
fn traces(level: i32) -> (Vec<String>, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.sav"), SAV).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_debug", "--nocapture", "--test-threads=1"])
        .env(CHILD, dir.path())
        .env(LEVEL, level.to_string())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let clean = |l: &str, dir: &Path| {
        let l = l.replace(&dir.display().to_string(), "DIR");
        match l.find("returned 0x") {
            Some(at) => format!("{}returned ADDR", &l[..at]),
            None => l,
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip_while(|l| !l.ends_with("BEGIN"))
        .skip(1)
        .take_while(|l| *l != "END")
        .map(|l| clean(l, dir.path()))
        .collect();
    let errlog = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.contains("dbrestore.c"))
        .map(|l| clean(l, dir.path()))
        .collect();
    (stdout, errlog)
}

#[test]
fn debug_1_traces_the_files_and_arrays() {
    let (stdout, errlog) = traces(1);
    assert_eq!(
        stdout,
        [
            "fopen_and_check: checkFile returned ADDR",
            "fopen_and_check: checkFile returned ADDR"
        ]
    );
    let file = [
        "../dbrestore.c:reboot_restore entry for file 's.sav'",
        "../dbrestore.c:reboot_restore Found filename 's.sav' in restoreFileList.",
    ];
    let header = "../dbrestore.c:reboot_restore header line '# autosave R5.10\tAutomatically generated - DO NOT MODIFY - 181018-120000";
    let mut expected = Vec::new();
    expected.extend(file);
    expected.extend([
        "../dbrestore.c:reboot_restore *** restoring from 'DIR/s.sav' at initHookState 6 (before record/device init) ***",
        header,
        "../dbrestore.c:SR_array_restore :entry: PV = 'W.VAL'",
        "../dbrestore.c:SR_array_restore p_data = (nil), p_data_size = 0",
        "../dbrestore.c:SR_array_restore No array write in pass 0.",
        "../dbrestore.c:reboot_restore writing boot-backup file 'DIR/s.sav.bu'.",
        "../dbrestore.c:reboot_restore done with file 's.sav'",
    ]);
    expected.extend(file);
    expected.extend([
        "../dbrestore.c:reboot_restore *** restoring from 'DIR/s.sav' at initHookState 7 (after record/device init) ***",
        header,
        "../dbrestore.c:SR_array_restore :entry: PV = 'W.VAL'",
        "../dbrestore.c:SR_array_restore p_data = (nil), p_data_size = 0",
        "../dbrestore.c:SR_array_restore Writing array to database",
        "../dbrestore.c:reboot_restore done with file 's.sav'",
    ]);
    assert_eq!(errlog, expected);
}

#[test]
fn debug_5_adds_the_per_line_traces() {
    let (stdout, errlog) = traces(5);
    let check = [
        "checkFile: entry",
        "checkFile: version=5.100000",
        "checkFile: files ends with '<END>'",
        "fopen_and_check: checkFile returned ADDR",
        "e name             rawval           value",
        "- ----             ------           -----",
        "  X                B                B",
    ];
    assert_eq!(stdout[..7], check, "{stdout:#?}");
    assert!(stdout.contains(&"reboot_restore: buffer='L.OUT $(X) NPP".to_string()));
    assert!(stdout.contains(&"                         ebuffer='L.OUT B NPP".to_string()));
    for line in [
        "../dbrestore.c:reboot_restore Current macro definitions:",
        "../dbrestore.c:myFileCopy copying 'DIR/s.sav' to 'DIR/s.sav.bu'",
        "../dbrestore.c:scalar_restore entry:field type 'DBF_OUTLINK'",
        "../dbrestore.c:SR_put_array_values restoring 2 values to W.VAL (max_elements=4)",
        "../dbrestore.c:scalar_restore Can't restore link field (L.OUT) in pass 1.",
    ] {
        assert!(errlog.contains(&line.to_string()), "{line}: {errlog:#?}");
    }
}
