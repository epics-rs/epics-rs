//! String text in a `.sav` is escaped as C writes it and decoded as C reads
//! it: a scalar through `epicsStrPrintEscaped` and `epicsStrnRawFromEscaped`
//! (save_restore.c:1861, dbrestore.c:235), an array element through
//! `SR_write_array_data`'s escapes (dbrestore.c:1391-1405). The input file
//! is the one the C oracle IOC restores and writes back.

use std::path::Path;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(stringout, "S") {}
record(waveform, "W") { field(FTVL, "STRING") field(NELM, "4") }
record(waveform, "H") { field(FTVL, "USHORT") field(NELM, "8") }
"#;

/// Boot-restores `line` from a C-written `s.sav`, then saves the set:
/// the restored value of `pv` and the line written for it.
async fn restore_and_save(pv: &'static str, line: &str) -> (EpicsValue, String) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir_all(d.join("req")).unwrap();
    std::fs::create_dir_all(d.join("sav")).unwrap();
    std::fs::write(d.join("a.db"), DB).unwrap();
    std::fs::write(d.join("req/s.req"), format!("{pv}\n")).unwrap();
    std::fs::write(
        d.join("sav/s.sav"),
        format!(
            "# Autosave release 6.1\tAutomatically generated - DO NOT MODIFY - 261018-125841\n\
             {line}\n<END>\n"
        ),
    )
    .unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}/req\")\n\
         set_savefile_path(\"{d}/sav\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         set_pass1_restoreFile(\"s.sav\")\n\
         create_monitor_set(\"s.req\", 5, \"\")\n\
         iocInit\n",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let restored = Arc::new(Mutex::new(None));
    let seen = restored.clone();
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(d.join("st.cmd").to_str().unwrap())
        .run(move |config| async move {
            *seen.lock().unwrap() = Some(config.db.get_pv(pv).unwrap());
            let mgr = config.autosave_manager.unwrap();
            mgr.manual_save("s.req", &config.db).await.unwrap();
            Ok(())
        })
        .await
        .unwrap();
    let value = restored.lock().unwrap().take().unwrap();
    (value, written_line(&d.join("sav/s.sav")))
}

fn written_line(sav: &Path) -> String {
    let text = std::fs::read_to_string(sav).unwrap();
    text.lines()
        .nth(1)
        .unwrap_or_else(|| panic!("{text}"))
        .to_string()
}

#[epics_macros_rs::epics_test]
async fn a_scalar_string_is_escaped_and_decoded_as_c_does() {
    let line = r#"S a\"b\\c\nd"#;
    let (value, written) = restore_and_save("S", line).await;
    assert_eq!(value, EpicsValue::String("a\"b\\c\nd".into()));
    assert_eq!(written, line);
}

/// C's reader takes `\"` and `\\` off an element and then decodes it
/// again (dbrestore.c:583-586, :623), so the C oracle restores `"p\\q"` as
/// `pq`; here an element is decoded once.
#[epics_macros_rs::epics_test]
async fn an_array_element_is_escaped_and_decoded_as_c_does() {
    let line = r#"W @array@ { "x\"y" "p\\q" "l1\nl2" }"#;
    let (value, written) = restore_and_save("W", line).await;
    assert_eq!(
        value,
        EpicsValue::StringArray(vec!["x\"y".into(), "p\\q".into(), "l1\nl2".into()].into())
    );
    assert_eq!(written, line);
}

/// C's escaped-string branch for a "DBF_CHAR" waveform (save_restore.c:1850)
/// takes a USHORT one, so the C oracle writes `H a` and cannot restore it;
/// here it stays an array.
#[epics_macros_rs::epics_test]
async fn a_ushort_waveform_round_trips_as_an_array() {
    let line = r#"H @array@ { "97" "98" "99" }"#;
    let (value, written) = restore_and_save("H", line).await;
    assert_eq!(value, EpicsValue::UShortArray(vec![97, 98, 99].into()));
    assert_eq!(written, line);
}
