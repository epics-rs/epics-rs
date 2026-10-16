#![cfg(unix)]
//! The `db*` reports print a name or a string field as its bytes: C writes
//! them with `%s`, and `dbpr`'s tab stops count those bytes. softIoc R7.0.10
//! on [`DB`]:
//!
//! ```text
//! dbl "" "DESC"   -> caf\xe9, "d\xe9sc"
//!                    al\xe9, "d\xe9sc"
//!                    o, ""
//! dbla            -> al\xe9 -> caf\xe9
//! dblsr o 1       -> Lock Set ... / o / caf\xe9
//! dbpr "caf\xe9"  -> AMSG:               ASG :               DESC: d\xe9sc          DISA: 0
//!                    DISV: 1             NAME: caf\xe9          NAMSG:              RVAL: 0
//! ```

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

const DB: &[u8] = b"record(ai, \"caf\xe9\") { field(DESC, \"d\xe9sc\") }
alias(\"caf\xe9\", \"al\xe9\")
record(ao, \"o\") { field(OUT, \"caf\xe9 PP\") }
";

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

fn lines(out: &[u8]) -> Vec<&[u8]> {
    out.split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .collect()
}

#[epics_macros_rs::epics_test]
async fn reports_print_latin1_names_and_values_as_bytes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("rep.db"), DB).unwrap();
    let db = Arc::new(PvDatabase::new());
    let load = [
        b"dbLoadRecords(\"".as_slice(),
        dir.path().join("rep.db").as_os_str().as_encoded_bytes(),
        b"\")",
    ]
    .concat();
    run(&db, dir.path(), &load);
    run(&db, dir.path(), b"iocInit");

    assert_eq!(
        run(&db, dir.path(), b"dbl \"\" \"DESC\""),
        b"caf\xe9, \"d\xe9sc\"\nal\xe9, \"d\xe9sc\"\no, \"\"\n"
    );
    assert_eq!(run(&db, dir.path(), b"dbla"), b"al\xe9 -> caf\xe9\n");

    let dblsr = run(&db, dir.path(), b"dblsr o 1");
    let mut members = lines(&dblsr)[1..].to_vec();
    members.sort();
    assert_eq!(members, [&b"caf\xe9"[..], b"o"]);

    let dbpr = run(&db, dir.path(), b"dbpr \"caf\xe9\"");
    assert_eq!(
        lines(&dbpr)[..2],
        [
            &b"AMSG:               ASG :               DESC: d\xe9sc          DISA: 0             "
                [..],
            b"DISV: 1             NAME: caf\xe9          NAMSG:              RVAL: 0             ",
        ]
    );
}
