#![cfg(unix)]
//! An iocsh argument is bytes: C reads the line with `fgets`, expands macros
//! and splits it as `char`, and hands each command its `sval` untouched. So a
//! Latin-1 macro value names a Latin-1 record and the UTF-8 spelling of the
//! same text does not. softIoc R7.0.10 on this script:
//!
//! ```text
//! dbLoadRecords("nb.db", "N=nu\xe9b")
//! iocInit
//! dbgf "nu\xe9b.DESC"            -> DBF_STRING:         "d\xe9sc"
//! dbpf "nu\xe9b.DESC" "x\xe9y"   -> DBF_STRING:         "x\xe9y"
//! dbgf "nu\xc3\xa9b.DESC"        -> PV 'nu\xc3\xa9b.DESC' not found
//! epicsEnvSet("V\xe9", "w\xe9")
//! epicsEnvShow("V\xe9")          -> V\xe9=w\xe9
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
async fn a_latin1_argument_reaches_the_command_as_its_bytes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("nb.db"),
        b"record(ai, \"$(N)\") {\n  field(DESC, \"d\xe9sc\")\n}\n",
    )
    .unwrap();
    let db = Arc::new(PvDatabase::new());
    let load = [
        b"dbLoadRecords(\"".as_slice(),
        dir.path().join("nb.db").as_os_str().as_encoded_bytes(),
        b"\", \"N=nu\xe9b\")",
    ]
    .concat();
    run(&db, dir.path(), &load);
    run(&db, dir.path(), b"iocInit");

    assert_eq!(
        run(&db, dir.path(), b"dbgf \"nu\xe9b.DESC\""),
        b"DBF_STRING:         \"d\\xe9sc\" \n"
    );
    assert_eq!(
        run(&db, dir.path(), b"dbpf \"nu\xe9b.DESC\" \"x\xe9y\""),
        b"DBF_STRING:         \"x\\xe9y\"  \n"
    );
    assert_eq!(
        run(&db, dir.path(), b"dbgf \"nu\xc3\xa9b.DESC\""),
        b"PV 'nu\xc3\xa9b.DESC' not found\n"
    );

    run(&db, dir.path(), b"epicsEnvSet(\"V\xe9\", \"w\xe9\")");
    assert_eq!(
        run(&db, dir.path(), b"epicsEnvShow(\"V\xe9\")"),
        b"V\xe9=w\xe9\n"
    );
}
