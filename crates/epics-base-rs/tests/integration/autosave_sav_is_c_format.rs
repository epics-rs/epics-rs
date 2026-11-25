//! An st.cmd IOC's `.sav` is the file C `write_it` (save_restore.c:1803-1886)
//! writes, so a C IOC restores it. The expected lines are the C oracle
//! IOC's for the same database, request file and puts.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "A") { field(VAL, "1.5") }
record(stringout, "S") { field(VAL, "hello world") }
record(mbbo, "M") { field(ZRST, "zero") field(ONST, "one") field(VAL, "1") }
record(waveform, "C") { field(FTVL, "CHAR") field(NELM, "20") }
"#;

#[epics_macros_rs::epics_test]
async fn an_st_cmd_ioc_writes_the_c_save_file() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir_all(d.join("req")).unwrap();
    std::fs::create_dir_all(d.join("sav")).unwrap();
    std::fs::write(d.join("a.db"), DB).unwrap();
    std::fs::write(d.join("req/s.req"), "A\nS\nM\nC\nNOPE\n").unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}/req\")\n\
         set_savefile_path(\"{d}/sav\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         create_monitor_set(\"s.req\", 5, \"\")\n\
         iocInit\n",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(d.join("st.cmd").to_str().unwrap())
        .run(|config| async move {
            // C's `dbpf C "abc"`: the string and its terminator.
            let abc = EpicsValue::CharArray(b"abc\0".to_vec().into());
            config.db.put_pv("C", abc).await.unwrap();
            let mgr = config.autosave_manager.unwrap();
            mgr.manual_save("s.req", &config.db).await.unwrap();
            Ok(())
        })
        .await
        .unwrap();

    let sav = std::fs::read_to_string(d.join("sav/s.sav")).unwrap();
    let lines: Vec<&str> = sav.lines().collect();
    // C's AUTOSAVE_RELEASE: `Autosave release 6.1, compiled <localtime>`.
    let (banner, date) = lines[0]
        .split_once("\tAutomatically generated - DO NOT MODIFY - ")
        .unwrap_or_else(|| panic!("{sav}"));
    let compiled = banner
        .strip_prefix("# Autosave release 6.1, compiled ")
        .unwrap_or_else(|| panic!("{sav}"));
    // Perl's `localtime`, e.g. `Sun Oct 18 09:52:34 2026`.
    let ctime = "%a %b %e %H:%M:%S %Y";
    let parsed = chrono::NaiveDateTime::parse_from_str(compiled, ctime)
        .unwrap_or_else(|e| panic!("{e}: {compiled:?}"));
    assert_eq!(parsed.format(ctime).to_string(), compiled);
    assert!(
        date.len() == 13 && date.as_bytes()[6] == b'-',
        "C fGetDateStr's %y%m%d-%H%M%S: {date}"
    );
    assert_eq!(
        lines[1..],
        [
            "! 1 channel(s) not connected - or not all gets were successful",
            "A 1.5",
            "S hello world",
            "M 1",
            "C @array@ { \"97\" \"98\" \"99\" \"0\" }",
            "#NOPE Search Issued",
            "<END>",
        ],
        "{sav}"
    );
}

/// C `readReqFile` keeps a PV listed twice (save_restore.c:3650-3672) and
/// `write_it` writes each of its channels; the C oracle IOC's file.
#[epics_macros_rs::epics_test]
async fn a_pv_listed_twice_is_saved_twice() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(
        d.join("a.db"),
        "record(ao, \"A\") { field(VAL, \"1.5\") }\nrecord(ao, \"B\") { field(VAL, \"2\") }\n",
    )
    .unwrap();
    std::fs::write(d.join("s.req"), "A\nB\nA\n").unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}\")\n\
         set_savefile_path(\"{d}\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         create_manual_set(\"s.req\", \"\")\n\
         iocInit\n",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(d.join("st.cmd").to_str().unwrap())
        .run(|config| async move {
            let mgr = config.autosave_manager.unwrap();
            mgr.manual_save("s.req", &config.db).await.unwrap();
            Ok(())
        })
        .await
        .unwrap();

    let sav = std::fs::read_to_string(d.join("s.sav")).unwrap();
    let lines: Vec<&str> = sav.lines().collect();
    assert_eq!(lines[1..], ["A 1.5", "B 2", "A 1.5", "<END>"], "{sav}");
}
