//! swait's dset is its own `devSWaitIoEvent`, whose `get_ioint_info` hands
//! `scanAdd` the `ioscanpvt` the record made (`swaitRecord.c:227-231`, 295),
//! so an I/O Intr swait stays I/O Intr, loaded so or put so. That list is
//! never scanned: an INxP input's monitor processes the record through
//! `inputChanged` and `ioIntProcess` (`:818-900`), once per change with the
//! value that change brought, and a change during the ODLY wait processes it
//! once more when the wait ends (`:801-805`).
//!
//! oracleSwaitIoc (calc R3-7-5), the [`IO_DB`] records, IO:N Passive then
//! `caput IO:N.SCAN "I/O Intr"`; no `scanAdd` line on the console:
//!
//! ```text
//! io scan W I/O Intr  N-put I/O Intr
//! io src=3 W 3
//! io src2=4 W 3
//! io src=5 W 9
//! io burst W 12  K 6 8 4  S 29                 SRC 6, 7, 8
//! io odly-wait D 2 3 1                         SRC3 1, then 2 and 3 in the wait
//! io odly-after D 3 3 0
//! io dol L 2 0 PV OK                           DSRC 9
//! ```
//!
//! The last line is C's `inputChanged` on DOL (`&pwait->inap + 12` reads
//! CALC), which the port does not copy: L stays 0.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

const IO_INTR: f64 = 2.0;
const PV_OK: f64 = 0.0;

const IO_DB: &str = r#"
record(ao, "IO:SRC") { }
record(ao, "IO:SRC2") { }
record(swait, "IO:W") { field(SCAN, "I/O Intr") field(INAN, "IO:SRC") field(INBN, "IO:SRC2") field(INBP, "No") field(CALC, "A+B") }
record(swait, "IO:K") { field(SCAN, "I/O Intr") field(INAN, "IO:SRC") field(INBN, "IO:SRC2") field(INBP, "No") field(CALC, "VAL+1") }
record(swait, "IO:S") { field(SCAN, "I/O Intr") field(INAN, "IO:SRC") field(CALC, "VAL+A") }
record(ao, "IO:SRC3") { }
record(swait, "IO:D") { field(SCAN, "I/O Intr") field(INAN, "IO:SRC3") field(CALC, "VAL+1") field(ODLY, "1") field(OOPT, "Every Time") }
record(ao, "IO:DSRC") { }
record(swait, "IO:L") { field(SCAN, "I/O Intr") field(DOLN, "IO:DSRC") field(CALC, "VAL+1") }
"#;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn put(db: &PvDatabase, rec: &str, v: f64) {
    db.put_record_field_from_ca(rec, "VAL", EpicsValue::Double(v))
        .await
        .unwrap();
}

async fn settle(db: &PvDatabase, done: impl Fn(&PvDatabase) -> bool) {
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while !done(db) && std::time::Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
}

fn wait(ms: u64) -> impl std::future::Future<Output = ()> {
    epics_base_rs::runtime::task::sleep(Duration::from_millis(ms))
}

#[epics_macros_rs::epics_test]
async fn an_io_intr_swait_stays_io_intr() {
    let ioc = IocBuilder::new()
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .db_string(
            r#"
record(swait, "IO:W") { field(SCAN, "I/O Intr") field(CALC, "1") }
record(swait, "IO:N") { field(CALC, "1") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = std::sync::Arc::clone(ioc.database());
    assert!(ioc.run().await);
    db.put_record_field_from_ca("IO:N", "SCAN", EpicsValue::String("I/O Intr".into()))
        .await
        .unwrap();
    assert_eq!(
        (num(&db, "IO:W.SCAN"), num(&db, "IO:N.SCAN")),
        (IO_INTR, IO_INTR),
        "IO:W.SCAN loaded I/O Intr, IO:N.SCAN put I/O Intr"
    );
}

#[epics_macros_rs::epics_test]
async fn an_input_change_processes_an_io_intr_record() {
    let ioc = IocBuilder::new()
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .db_string(IO_DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = std::sync::Arc::clone(ioc.database());
    assert!(ioc.run().await);
    let statuses = [
        "IO:W.INAV",
        "IO:W.INBV",
        "IO:K.INAV",
        "IO:K.INBV",
        "IO:S.INAV",
        "IO:D.INAV",
        "IO:L.DOLV",
    ];
    settle(&db, |db| statuses.iter().all(|f| num(db, f) == PV_OK)).await;

    // A and the process it causes are one hold, so A read back means the
    // process ran; K and S count from here.
    put(&db, "IO:SRC", 3.0).await;
    settle(&db, |db| {
        ["IO:W.A", "IO:K.A", "IO:S.A"]
            .iter()
            .all(|f| num(db, f) == 3.0)
    })
    .await;
    assert_eq!(num(&db, "IO:W"), 3.0, "W after SRC 3");
    let (k0, s0) = (num(&db, "IO:K"), num(&db, "IO:S"));
    put(&db, "IO:SRC2", 4.0).await;
    wait(300).await;
    assert_eq!(num(&db, "IO:W"), 3.0, "W after SRC2 4, an INBP No input");
    put(&db, "IO:SRC", 5.0).await;
    settle(&db, |db| num(db, "IO:W.A") == 5.0).await;
    assert_eq!(num(&db, "IO:W"), 9.0, "W after SRC 5");
    for v in [6.0, 7.0, 8.0] {
        put(&db, "IO:SRC", v).await;
    }
    settle(&db, |db| {
        ["IO:W.A", "IO:K.A", "IO:S.A"]
            .iter()
            .all(|f| num(db, f) == 8.0)
    })
    .await;
    assert_eq!(
        (
            num(&db, "IO:W"),
            num(&db, "IO:K") - k0,
            num(&db, "IO:K.B"),
            num(&db, "IO:S") - s0
        ),
        (12.0, 4.0, 4.0, 26.0),
        "after SRC 6, 7, 8: W, K's processes since SRC 3, K.B, S's sum of A since SRC 3"
    );

    settle(&db, |db| {
        num(db, "IO:D") == 1.0 && num(db, "IO:D.PACT") == 0.0
    })
    .await;
    put(&db, "IO:SRC3", 1.0).await;
    settle(&db, |db| num(db, "IO:D.PACT") == 1.0).await;
    put(&db, "IO:SRC3", 2.0).await;
    put(&db, "IO:SRC3", 3.0).await;
    settle(&db, |db| num(db, "IO:D.A") == 3.0).await;
    let in_wait = (num(&db, "IO:D"), num(&db, "IO:D.PACT"));
    settle(&db, |db| {
        num(db, "IO:D") == 3.0 && num(db, "IO:D.PACT") == 0.0
    })
    .await;
    assert_eq!(
        (in_wait, (num(&db, "IO:D"), num(&db, "IO:D.A"))),
        ((2.0, 1.0), (3.0, 3.0)),
        "D (VAL, PACT) in the ODLY wait after SRC3 1, 2, 3; (VAL, A) after it"
    );

    put(&db, "IO:DSRC", 9.0).await;
    wait(300).await;
    assert_eq!(
        (num(&db, "IO:L"), num(&db, "IO:L.LA")),
        (0.0, 0.0),
        "L (VAL, LA) after DSRC 9"
    );
}
