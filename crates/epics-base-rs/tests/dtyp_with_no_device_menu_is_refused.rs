//! A string put to the DTYP of a record type with no device support is
//! refused by `cvt_st_device` (`dbFastLinkConv.c:330-348`: no menu, no
//! choice), and `cvt_device_st` reads that DTYP back as "" (`:1615-1618`).
//! softIoc R7.0.10.1 on `calc C`, `stringout SO` (OUT `C.DTYP`):
//!
//! ```text
//! dbgf C.DTYP         DBF_STRING:         ""
//! dbpf C.DTYP Bogus   recGblDbaddrError: dbFastLinkConv(cvt_st_device) Illegal choice PV: C.DTYP
//! dbpf C.DTYP 0       recGblDbaddrError: dbFastLinkConv(cvt_st_device) Illegal choice PV: C.DTYP
//! dbpf SO Bogus       the same line, then SO LINK/INVALID
//! dbpf SO 0           the same line
//! ```
//!
//! Unix only: the `dbgf` rendering asserted is the process console.

#![cfg(unix)]

use std::collections::HashMap;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};

use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(calc, "C") {}
record(stringout, "SO") { field(OUT, "C.DTYP") }
"#;

const LINE: &str = "recGblDbaddrError: dbFastLinkConv(cvt_st_device) Illegal choice PV: C.DTYP";

#[epics_macros_rs::epics_test]
async fn a_dtyp_with_no_device_menu_refuses_every_string() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&heard);
    errlog_add_listener(move |m| sink.lock().unwrap().push(m.trim_end().to_string()));
    let heard = move || {
        errlog_flush();
        let said: Vec<String> = std::mem::take(&mut *heard.lock().unwrap())
            .into_iter()
            .filter(|m| m.starts_with("recGbl"))
            .collect();
        said
    };
    heard();
    for put in ["Bogus", "0"] {
        let refused = db
            .put_record_field_from_ca("C", "DTYP", EpicsValue::String(put.into()))
            .await;
        assert!(refused.is_err(), "CA {put}: {refused:?}");
        assert_eq!(heard(), [LINE], "CA {put}");

        db.put_record_field_from_ca("SO", "VAL", EpicsValue::String(put.into()))
            .await
            .unwrap();
        assert_eq!(heard(), [LINE], "link {put}");
        let num = |f: &str| db.get_pv(format!("SO.{f}")).unwrap().to_f64();
        assert_eq!(
            [num("STAT"), num("SEVR")],
            [Some(14.0), Some(3.0)],
            "link {put}"
        );
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('\x1b') {
        out.push_str(&rest[..at]);
        rest = match rest[at..].find('m') {
            Some(end) => &rest[at + end + 1..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// What the shell printed on stdout running `lines` after the load.
fn console(lines: &str) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("t.db"), DB).expect("write the .db");
    let script = dir.path().join("t.cmd");
    std::fs::write(
        &script,
        format!("dbLoadRecords(\"t.db\")\niocInit\n{lines}"),
    )
    .expect("write the script");

    let out_sink = tempfile::NamedTempFile::new().expect("stdout capture");
    // SAFETY: the shell writes through fd 1 for the length of one script and
    // this test is `serial`, so nothing else holds it.
    let saved = unsafe {
        std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path());
        let saved = libc::dup(1);
        libc::dup2(out_sink.as_file().as_raw_fd(), 1);
        saved
    };
    let db = Arc::new(PvDatabase::new());
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let path = script.display().to_string();
    let ran = std::thread::spawn(move || IocShell::new(db, bridge).execute_script(&path)).join();
    let _ = std::io::stdout().flush();
    // SAFETY: restoring the descriptor this call replaced.
    unsafe {
        libc::dup2(saved, 1);
        libc::close(saved);
        std::env::remove_var("EPICS_DB_INCLUDE_PATH");
    }
    assert!(ran.is_ok(), "the shell panicked");
    strip_ansi(&std::fs::read_to_string(out_sink.path()).expect("stdout capture"))
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_dtyp_with_no_device_menu_reads_empty() {
    let out = console("dbgf C.DTYP\n");
    let want = "dbgf C.DTYP\nDBF_STRING:         \"\"        \n";
    assert!(out.contains(want), "dbgf C.DTYP:\n{out}");
}
