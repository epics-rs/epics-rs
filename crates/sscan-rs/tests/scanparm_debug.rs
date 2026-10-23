//! `var scanparmRecordDebug 1` turns on `scanparmRecord.c`'s process traces.
//!
//! oracleSscanIoc (sscan R2-12) on `P` aimed at local stand-ins, after
//! `var scanparmRecordDebug 1`: `dbpf P.LOAD 1`, `dbpf P.GO 1`,
//! `dbpf S:busy 1`, `dbpf P.GO 1` print
//!
//! ```text
//! scanparm(P):process:act=0
//! scanparm(P):process:mp=100
//! scanparm(P):process:act=0
//! scanparm(P):process:mp=100
//! scanparm(P):process:starting scan
//! scanparm(P):process:act=1
//! scanparm(P):process:mp=100
//! ```
//!
//! With OSP a CA link to no PV, the LOAD group's last put fails and C
//! returns before "starting scan" (`scanparmRecord.c:206-211`): `dbpf P.GO 1`
//! prints only `act=0` and `mp=100`.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::records::{bi::BiRecord, longin::LonginRecord, longout::LongoutRecord};
use epics_base_rs::types::EpicsValue;

const CHILD: &str = "SCANPARM_DEBUG_CHILD";

/// The database, with OSP `osp`.
fn db(osp: &str) -> String {
    format!(
        r#"
record(bi, "S:busy") {{}}
record(longin, "S:mpts") {{ field(VAL, "100") }}
record(longout, "S:sp") {{}}
record(longout, "S:go") {{}}
record(longout, "S:load") {{}}
record(scanparm, "P") {{
  field(SP, "-1")
  field(OSP, "{osp}")
  field(OGO, "S:go PP")
  field(OLOAD, "S:load PP")
  field(IMP, "S:mpts NPP")
  field(IACT, "S:busy NPP")
}}
"#
    )
}

/// This test run again in a child whose stdout is not captured, and the
/// scanparm lines it printed.
fn trace_of(test: &str) -> Vec<String> {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        // libtest's `test <name> ... ` heads the first line, unterminated.
        .filter_map(|l| l.find("scanparm(").map(|i| l[i..].to_string()))
        .collect()
}

async fn ioc(osp: &str) -> Arc<PvDatabase> {
    let (name, factory) = sscan_rs::scanparm_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_record_type("bi", || Box::new(BiRecord::default()))
        .register_record_type("longin", || Box::new(LonginRecord::default()))
        .register_record_type("longout", || Box::new(LongoutRecord::default()))
        .db_string(&db(osp), &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db
}

/// The put's cycle runs past the put's return: wait for it to clear LOAD
/// and GO and leave PACT.
async fn settled(db: &PvDatabase) {
    let f = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    for _ in 0..500 {
        if f("P.PACT") == 0.0 && f("P.LOAD") == 0.0 && f("P.GO") == 0.0 {
            return;
        }
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("P never settled");
}

/// One iocsh line, on a thread of its own as the shell runs.
fn iocsh(db: &Arc<PvDatabase>, line: &str) {
    let (db, line) = (db.clone(), line.to_string());
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    std::thread::spawn(move || IocShell::new(db, bridge).execute_line_reported(&line))
        .join()
        .unwrap()
        .unwrap();
}

#[epics_base_rs::epics_test]
async fn the_debug_variable_prints_the_process_traces() {
    if std::env::var_os(CHILD).is_none() {
        assert_eq!(
            trace_of("the_debug_variable_prints_the_process_traces"),
            [
                "scanparm(P):process:act=0",
                "scanparm(P):process:mp=100",
                "scanparm(P):process:act=0",
                "scanparm(P):process:mp=100",
                "scanparm(P):process:starting scan",
                "scanparm(P):process:act=1",
                "scanparm(P):process:mp=100",
            ]
        );
        return;
    }
    let db = ioc("S:sp PP").await;
    // Off by default: this LOAD prints nothing.
    db.put_record_field_from_ca("P", "LOAD", EpicsValue::Short(1))
        .await
        .unwrap();
    settled(&db).await;
    iocsh(&db, "var scanparmRecordDebug 1");
    for (rec, field) in [("P", "LOAD"), ("P", "GO"), ("S:busy", "VAL"), ("P", "GO")] {
        db.put_record_field_from_ca(rec, field, EpicsValue::Short(1))
            .await
            .unwrap();
        settled(&db).await;
    }
}

#[epics_base_rs::epics_test]
async fn a_load_group_that_returns_does_not_print_starting_scan() {
    if std::env::var_os(CHILD).is_none() {
        assert_eq!(
            trace_of("a_load_group_that_returns_does_not_print_starting_scan"),
            ["scanparm(P):process:act=0", "scanparm(P):process:mp=100"]
        );
        return;
    }
    let db = ioc("NO:SUCH CA").await;
    iocsh(&db, "var scanparmRecordDebug 1");
    let put = db
        .put_record_field_from_ca_no_notify("P", "GO", EpicsValue::Short(1))
        .await;
    assert!(put.is_err(), "OSP's put fails and process() returns it");
}
