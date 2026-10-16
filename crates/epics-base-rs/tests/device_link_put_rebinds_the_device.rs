//! A runtime INP/OUT put to a device whose dset extends itself runs the new
//! dset's `add_record` on the new link (`dbAccess.c:1196`), so the device acts
//! on that link from the next process on, and a refusal leaves the record with
//! no dset and PACT set. Every value is `softIoc` R7.0.10-146 (8f5015b66) on
//! the same database and puts, with `ENVA=aaa ENVB=bbb`.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(bi, "DS") { field(DTYP, "Db State") field(INP, "@st1") field(ZNAM, "zero") field(ONAM, "one") }
record(bo, "SET2") { field(DTYP, "Db State") field(OUT, "@st2") field(VAL, "1") }
record(lso, "LS") { field(DTYP, "stdio") field(OUT, "@stdout") }
record(stringin, "GE") { field(DTYP, "getenv") field(INP, "@ENVA") }
record(stringin, "TS") { field(DTYP, "Soft Timestamp") field(INP, "@%Y") }
"#;

const CHILD: &str = "DEVICE_LINK_PUT_CHILD";

async fn put(db: &PvDatabase, pv: &str, text: &str) -> bool {
    let (rec, field) = pv.split_once('.').unwrap_or((pv, "VAL"));
    db.put_record_field_from_ca_no_notify(rec, field, EpicsValue::String(text.into()))
        .await
        .is_ok()
}

fn get(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        v => v.to_f64().unwrap().to_string(),
    }
}

/// `strftime` of now through `date`, the clock C's `epicsTimeToStrftime` reads.
fn date(format: &str) -> String {
    let out = std::process::Command::new("date")
        .arg(format)
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[epics_macros_rs::epics_test]
async fn child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let db: Arc<PvDatabase> = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    assert!(put(&db, "SET2.PROC", "1").await);

    assert!(put(&db, "DS.PROC", "1").await);
    assert_eq!(get(&db, "DS"), "0"); // ZNAM "zero"
    assert!(put(&db, "DS.INP", "@st2").await);
    assert!(put(&db, "DS.PROC", "1").await);
    assert_eq!(get(&db, "DS"), "1"); // ONAM "one"

    assert!(put(&db, "LS", "hello1").await);
    assert!(put(&db, "LS.OUT", "@stderr").await);
    assert!(put(&db, "LS", "hello2").await);

    assert!(put(&db, "GE.PROC", "1").await);
    assert_eq!(get(&db, "GE"), "aaa");
    assert!(put(&db, "GE.INP", "@ENVB").await);
    assert!(put(&db, "GE.PROC", "1").await);
    assert_eq!(get(&db, "GE"), "bbb");

    assert!(put(&db, "TS.PROC", "1").await);
    assert_eq!(get(&db, "TS"), date("+%Y"));
    assert!(put(&db, "TS.INP", "@%m").await);
    assert!(put(&db, "TS.PROC", "1").await);
    assert_eq!(get(&db, "TS"), date("+%m"));

    // `add_lso` refuses a stream outside `outStreams` without a word: the
    // link moves, DSET and DPVT go NULL, PACT 1; `@stdout` binds again.
    assert!(!put(&db, "LS.OUT", "@nosuch").await);
    assert_eq!(get(&db, "LS.OUT"), "@nosuch");
    assert_eq!(get(&db, "LS.PACT"), "1");
    let detached = |db: &PvDatabase| {
        db.get_record("LS")
            .unwrap()
            .read()
            .common
            .dtyp
            .is_detached()
    };
    assert!(detached(&db));
    assert!(put(&db, "LS.OUT", "@stdout").await);
    assert_eq!(get(&db, "LS.PACT"), "0");
    assert!(!detached(&db));
}

#[test]
fn a_device_link_put_binds_the_new_link() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("ENVA", "aaa")
        .env("ENVB", "bbb")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let lines = |b: &[u8]| -> Vec<String> {
        String::from_utf8_lossy(b)
            .lines()
            // The harness's `test child ... ` shares the first line.
            .filter_map(|l| l.find("hello").map(|at| l[at..].to_string()))
            .collect()
    };
    assert_eq!(lines(&out.stdout), ["hello1"]);
    assert_eq!(lines(&out.stderr), ["hello2"]);
}
