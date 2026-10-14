//! A device object's `DPVT` row. `softIoc` 8f5015b66 on this database, after
//! one `dbpf <rec>.PROC 1` each, prints `dbpr <rec> 4`'s DPVT set where the
//! dset's `add_record` bound a state or stream and `(nil)` where it bound
//! none or keeps no dpvt.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;

const DB: &str = r#"
record(bi, "DS") { field(DTYP, "Db State") field(INP, "@st1") }
record(bi, "DE") { field(DTYP, "Db State") field(INP, "@") }
record(lso, "LS") { field(DTYP, "stdio") field(OUT, "@stdout") }
record(lso, "LN") { field(DTYP, "stdio") field(OUT, "@nowhere") }
record(stringin, "GE") { field(DTYP, "getenv") field(INP, "@HOME") }
record(bo, "BS") { field(DTYP, "Db State") field(OUT, "@st2") }
record(printf, "PF") { field(DTYP, "stdio") field(OUT, "@stderr") }
"#;

const C: [(&str, bool); 7] = [
    ("DS", true),
    ("DE", false),
    ("LS", true),
    ("LN", false),
    ("GE", false),
    ("BS", true),
    ("PF", true),
];

fn run(shell: &IocShell, line: &str) -> String {
    let out = std::env::temp_dir().join(format!("dbpr-dpvt-{}", std::process::id()));
    shell
        .execute_line(format!("{line} > {}", out.display()))
        .unwrap();
    let text = std::fs::read_to_string(&out).unwrap();
    std::fs::remove_file(&out).unwrap();
    text
}

// RTEMS-EXEC-MODEL-ALLOW(1): the runtime only feeds the build and
// `BlockingBridge::capture` for the shell; no CA server is built.
#[test]
fn dbpr_prints_a_device_object_s_dpvt_where_c_sets_one() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (db, bridge) = rt.block_on(async {
        let (db, _) = IocBuilder::new()
            .db_string(DB, &HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();
        (db, epics_base_rs::runtime::task::BlockingBridge::capture())
    });
    let shell = IocShell::new(Arc::clone(&db), bridge);
    for (rec, _) in C {
        run(&shell, &format!("dbpf {rec}.PROC 1"));
    }
    for (rec, set) in C {
        let printed = run(&shell, &format!("dbpr {rec} 4"));
        let nil = printed.contains("DPVT: PTR (nil)");
        assert!(printed.contains("DPVT: PTR"), "{rec}: {printed}");
        assert_eq!(!nil, set, "{rec}: {printed}");
    }
    drop(shell);
    std::mem::forget(rt);
}
