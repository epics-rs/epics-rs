// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — its one hand-built tokio runtime ran in a suite that passed under `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p epics-base-rs --test db_load_records_hook` (1/1).
//! C `dbLoadRecordsHook` (`dbAccess.c:74`, `:804-807`): a `dbLoadRecords`
//! that read its file calls the hook with the file and the raw
//! substitution string, and so does each `dbLoadTemplate` row, which is a
//! `dbLoadRecords` of the row's `sub_collect` (`dbLoadTemplate.y:49-57`).

use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::{IocShell, set_db_load_records_hook};

#[test]
fn a_load_that_read_its_file_calls_the_hook() {
    let dir = tempfile::tempdir().unwrap();
    let db_file = dir.path().join("a.db");
    std::fs::write(&db_file, "record(ao, \"$(P)$(N=0)\") {}\n").unwrap();
    let subs = dir.path().join("a.substitutions");
    std::fs::write(
        &subs,
        format!(
            "file \"{}\" {{\npattern {{N}}\n{{1}}\n{{\"2\"}}\n}}\n",
            db_file.display()
        ),
    )
    .unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    set_db_load_records_hook(Some(Arc::new(move |file: &str, subs: &str| {
        seen.lock().unwrap().push(format!("{file}|{subs}"));
    })));
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    let db = Arc::new(PvDatabase::new());
    let (dbf, subf) = (db_file.display().to_string(), subs.display().to_string());
    std::thread::spawn(move || {
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        let shell = IocShell::new(db, bridge);
        for line in [
            format!("dbLoadRecords(\"{dbf}\", \"P=x,N=a\")"),
            "dbLoadRecords(\"nosuch.db\", \"P=y\")".to_string(),
            format!("dbLoadTemplate(\"{subf}\", \"P=t\")"),
        ] {
            let _ = shell.execute_line(&line);
        }
    })
    .join()
    .unwrap();
    let d = db_file.display();
    assert_eq!(
        *calls.lock().unwrap(),
        [
            format!("{d}|P=x,N=a"),
            format!("{d}|P=t,N=1"),
            format!("{d}|P=t,N=\"2\""),
        ]
    );
}
