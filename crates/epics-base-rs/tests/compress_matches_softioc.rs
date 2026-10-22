//! compress against softIoc (R7.0.10.1-DEV): every ALG, both BALG, PBUF,
//! ILIL/IHIL, a scalar source and an INP length change, fed through FLNK
//! chains. `fixtures/softioc/compress.out` is softIoc's `dbgf` output for
//! `compress.cmds` over `compress.db`.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;

const DB: &str = include_str!("fixtures/softioc/compress.db");
const CMDS: &str = include_str!("fixtures/softioc/compress.cmds");
const C_OUT: &str = include_str!("fixtures/softioc/compress.out");

#[epics_macros_rs::epics_test]
async fn compress_cycles_match_softioc_compressrecord_c_process() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    // The shell blocks on the runtime, so it runs on a thread of its own.
    let rs = std::thread::spawn(move || {
        let shell = IocShell::new(db, bridge);
        let out = tempfile::NamedTempFile::new().unwrap();
        // iocsh reads an unquoted `\` as an escape, as C `split()` does.
        let token = out.path().display().to_string().replace('\\', "/");
        let mut rs = String::new();
        for line in CMDS.lines() {
            shell.execute_line(format!("{line} > {token}")).unwrap();
            rs.push_str(&std::fs::read_to_string(out.path()).unwrap());
        }
        rs
    })
    .join()
    .unwrap();
    if rs != C_OUT {
        let diff: Vec<_> = rs
            .lines()
            .zip(C_OUT.lines())
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .take(10)
            .map(|(i, (a, b))| format!("{}: rs {a:?}\n    c  {b:?}", i + 1))
            .collect();
        panic!(
            "{} rs lines vs {} C lines\n{}",
            rs.lines().count(),
            C_OUT.lines().count(),
            diff.join("\n")
        );
    }
}
