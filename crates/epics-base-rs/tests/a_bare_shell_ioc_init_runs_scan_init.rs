//! An `iocInit` in a shell with no `IocApplication` behind it — a bare
//! `PvDatabase` shell — runs C `scanInit` over the records its `st.cmd`
//! loaded, as every `iocInit` does (`iocInit.c:186`): an I/O Intr record
//! whose dset has no `get_ioint_info` is refused with C's line and left
//! Passive, and a later SCAN put reaches `scanAdd` again.

use std::sync::{Arc, Mutex};

use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::record::ScanType;
use epics_base_rs::types::EpicsValue;

#[test]
fn a_bare_shell_ioc_init_refuses_an_io_intr_record_without_a_hook() {
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&heard);
    errlog_add_listener(move |m| sink.lock().unwrap().push(m.to_string()));
    let said = || {
        errlog_flush();
        std::mem::take(&mut *heard.lock().unwrap())
            .iter()
            .flat_map(|m| m.lines().map(str::to_string).collect::<Vec<_>>())
            .filter(|l| l.contains("scanAdd"))
            .collect::<Vec<_>>()
    };

    let dir = tempfile::tempdir().unwrap();
    let db_file = dir.path().join("intr.db");
    std::fs::write(
        &db_file,
        "record(ai, \"INTR\") { field(SCAN, \"I/O Intr\") }\n",
    )
    .unwrap();
    let st_cmd = dir.path().join("st.cmd");
    std::fs::write(
        &st_cmd,
        format!("dbLoadRecords(\"{}\")\niocInit\n", db_file.display()),
    )
    .unwrap();

    // RTEMS-EXEC-MODEL-ALLOW(1): the runtime only captures the shell's bridge and drives one put; passes in the exec-backend suite.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    let db = Arc::new(PvDatabase::new());
    let shell_db = db.clone();
    let path = st_cmd.to_str().unwrap().to_string();
    std::thread::spawn(move || IocShell::new(shell_db, bridge).execute_script(path))
        .join()
        .unwrap()
        .unwrap();

    let line = "recGblRecordError: scanAdd: I/O Intr not valid (no get_ioint_info)  PV: INTR";
    assert_eq!(said(), [line]);
    let scan = || db.get_record("INTR").unwrap().read().common.scan;
    assert_eq!(scan(), ScanType::Passive);

    rt.block_on(db.put_pv("INTR.SCAN", EpicsValue::String("I/O Intr".into())))
        .unwrap();
    assert_eq!(said(), [line], "a SCAN put after iocInit");
    assert_eq!(scan(), ScanType::Passive);

    assert_eq!(epics_base_rs::server::ioc_app::ioc_shutdown(), 0);
}
