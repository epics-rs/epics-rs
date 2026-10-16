#![cfg(unix)]
//! An event name is bytes: C `eventNameToHandle` `strcmp`s EVNT against
//! `pevent_list` (`dbScan.c:493-532`), so two names that differ in one
//! Latin-1 byte are two events. softIoc R7.0.10 on this script, with `a` and
//! `b` calcs (`CALC "VAL+1"`) on `SCAN "Event"`, EVNT `caf\xe9` and `caf\xe8`:
//!
//! ```text
//! postEvent "caf\xe9"
//! dbgf a                 -> DBF_DOUBLE:         1
//! dbgf b                 -> DBF_DOUBLE:         0
//! scanpel "caf\xe9"      -> Event "caf\xe9"
//!                            Priority Low
//!                               a
//! ```

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

fn run(db: &Arc<PvDatabase>, dir: &std::path::Path, line: &[u8]) -> Vec<u8> {
    let out = dir.join("out");
    let line = [line, b" > ", out.as_os_str().as_encoded_bytes()].concat();
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let shell_db = Arc::clone(db);
    std::thread::spawn(move || IocShell::new(shell_db, bridge).execute_line(line))
        .join()
        .unwrap()
        .unwrap();
    std::fs::read(&out).unwrap()
}

#[epics_macros_rs::epics_test]
async fn a_latin1_event_name_posts_only_its_own_bytes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("ev.db"),
        b"record(calc, \"a\") { field(SCAN, \"Event\") field(EVNT, \"caf\xe9\") field(CALC, \"VAL+1\") }
record(calc, \"b\") { field(SCAN, \"Event\") field(EVNT, \"caf\xe8\") field(CALC, \"VAL+1\") }
",
    )
    .unwrap();
    let db = Arc::new(PvDatabase::new());
    let load = [
        b"dbLoadRecords(\"".as_slice(),
        dir.path().join("ev.db").as_os_str().as_encoded_bytes(),
        b"\")",
    ]
    .concat();
    run(&db, dir.path(), &load);
    run(&db, dir.path(), b"iocInit");
    // A bare database's `iocInit` leaves the running transition to its owner.
    db.scan_control().run();
    run(&db, dir.path(), b"postEvent \"caf\xe9\"");

    let val = |name: &str| db.get_record(name).unwrap().read().record.val();
    assert_eq!(val("a").and_then(|v| v.to_f64()), Some(1.0));
    assert_eq!(val("b").and_then(|v| v.to_f64()), Some(0.0));
    assert_eq!(
        run(&db, dir.path(), b"scanpel \"caf\xe9\""),
        [
            &b"Event \"caf\xe9\"\n Priority Low\n    a"[..],
            &[b' '; 27],
            b"\n"
        ]
        .concat()
    );
}
