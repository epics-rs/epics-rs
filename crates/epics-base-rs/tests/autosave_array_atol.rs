//! An array member's element texts become numbers as C's own conversions
//! make them, not by a strict parse: at boot by the field's type,
//! `atol`, `strtoul(.., 0)`, `epicsParseInt64` and `atof` (dbrestore.c:
//! 520-536, :620-645); by `fdbrestore` through the CA type's `atol` or
//! `atof` (save_restore.c:3002-3007, :3102-3107). The values are the C
//! oracle IOC's `dbgf`s after each restore of the same file.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::sleep;

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::types::EpicsValue;

const CHILD: &str = "AUTOSAVE_ARRAY_ATOL_SCRIPT";
const TYPES: [&str; 10] = [
    "LONG", "SHORT", "USHORT", "ULONG", "CHAR", "UCHAR", "FLOAT", "DOUBLE", "INT64", "UINT64",
];

/// Each record's `name value,value,..` line, in the child only.
#[epics_macros_rs::epics_test]
async fn child_restore() {
    let Some(script) = std::env::var_os(CHILD) else {
        return;
    };
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(script.to_str().unwrap())
        .run(|config| async move {
            let db = config.db.clone();
            epics_base_rs::server::db_server::announce_serving("probe");
            let mgr = config.autosave_manager.unwrap();
            // The script's fdbrestore is queued once its line has run.
            let deadline = Instant::now() + Duration::from_secs(10);
            while db.get_pv("BUINT64").unwrap().count() == 0 && Instant::now() < deadline {
                sleep(Duration::from_millis(20)).await;
            }
            mgr.ops_served().await;
            for t in TYPES {
                for name in [format!("A{t}"), format!("B{t}")] {
                    let text = match db.get_pv(&name).unwrap() {
                        EpicsValue::LongArray(a) => join(a.iter()),
                        EpicsValue::ShortArray(a) => join(a.iter()),
                        EpicsValue::UShortArray(a) => join(a.iter()),
                        EpicsValue::ULongArray(a) => join(a.iter()),
                        EpicsValue::CharArray(a) => join(a.iter()),
                        EpicsValue::UCharArray(a) => join(a.iter()),
                        EpicsValue::FloatArray(a) => join(a.iter()),
                        EpicsValue::DoubleArray(a) => join(a.iter()),
                        EpicsValue::Int64Array(a) => join(a.iter()),
                        EpicsValue::UInt64Array(a) => join(a.iter()),
                        other => format!("{other:?}"),
                    };
                    println!("VALUE {name} {text}");
                }
            }
            mgr.shutdown();
            Ok(())
        })
        .await
        .unwrap();
}

fn join<T: std::fmt::Display>(values: impl Iterator<Item = T>) -> String {
    values.map(|v| v.to_string()).collect::<Vec<_>>().join(",")
}

/// The child's values after `before_init` and `after_init`.
fn restored(before_init: &str, after_init: &str) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir_all(d.join("sav")).unwrap();
    let mut db = String::new();
    let mut sav = String::from("# Autosave release 6.1\n");
    for t in TYPES {
        for r in ["A", "B"] {
            db += &format!(
                "record(waveform, \"{r}{t}\") {{ field(FTVL, \"{t}\") field(NELM, \"8\") }}\n"
            );
        }
        sav += &format!(
            "A{t} @array@ {{ \"3x\" \"5 6\" \"-1\" \"0x10\" \"010\" \"70000\" \"1e3\" \"4000000000\" }}\nB{t} 5 6\n"
        );
    }
    sav += "<END>\n";
    std::fs::write(d.join("a.db"), db).unwrap();
    std::fs::write(d.join("sav/s.sav"), sav).unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_savefile_path(\"{d}/sav\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         {before_init}\
         create_manual_set(\"\")\n\
         iocInit\n\
         {after_init}",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_restore",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, d.join("st.cmd"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once("VALUE ").map(|(_, v)| v.to_string()))
        .collect()
}

#[test]
fn a_boot_restore_converts_each_element_as_c() {
    assert_eq!(
        restored("set_pass1_restoreFile(\"s.sav\")\n", ""),
        [
            "ALONG 3,5,-1,0,10,70000,1,-294967296",
            "BLONG 5",
            "ASHORT 3,5,-1,0,10,4464,1,10240",
            "BSHORT 5",
            "AUSHORT 3,5,65535,0,10,4464,1,10240",
            "BUSHORT 5",
            "AULONG 3,5,4294967295,16,8,70000,1,4000000000",
            "BULONG 5",
            "ACHAR 3,5,255,0,10,112,1,0",
            "BCHAR 5",
            "AUCHAR 3,5,255,0,10,112,1,0",
            "BUCHAR 5",
            "AFLOAT 3,5,-1,16,10,70000,1000,4000000000",
            "BFLOAT 5",
            "ADOUBLE 3,5,-1,16,10,70000,1000,4000000000",
            "BDOUBLE 5",
            "AINT64 0,0,-1,0,10,70000,0,4000000000",
            "BINT64 0",
            "AUINT64 0,0,18446744073709551615,0,10,70000,0,4000000000",
            "BUINT64 0",
        ]
    );
}

/// The 64-bit and ULONG fields take the CA `DBR_DOUBLE`'s `atof`.
#[test]
fn a_manual_restore_converts_each_element_as_c() {
    assert_eq!(
        restored("", "fdbrestore(\"s.sav\")\n"),
        [
            "ALONG 3,5,-1,0,10,70000,1,-294967296",
            "BLONG 5",
            "ASHORT 3,5,-1,0,10,4464,1,10240",
            "BSHORT 5",
            "AUSHORT 3,5,65535,0,10,4464,1,10240",
            "BUSHORT 5",
            "AULONG 3,5,4294967295,16,10,70000,1000,4000000000",
            "BULONG 5",
            "ACHAR 3,5,255,0,10,112,1,0",
            "BCHAR 5",
            "AUCHAR 3,5,255,0,10,112,1,0",
            "BUCHAR 5",
            "AFLOAT 3,5,-1,16,10,70000,1000,4000000000",
            "BFLOAT 5",
            "ADOUBLE 3,5,-1,16,10,70000,1000,4000000000",
            "BDOUBLE 5",
            "AINT64 3,5,-1,16,10,70000,1000,4000000000",
            "BINT64 5",
            "AUINT64 3,5,18446744073709551615,16,10,70000,1000,4000000000",
            "BUINT64 5",
        ]
    );
}
