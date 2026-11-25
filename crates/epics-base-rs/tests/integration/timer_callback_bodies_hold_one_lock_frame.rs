//! A timer callback whose C body runs under one `dbScanLock` runs in one
//! frame of the record's lock set here too.
//!
//! C `boRecord.c::myCallbackFunc` (:107-119, `busyRecord.c:112-124`) takes
//! `dbScanLock`, writes VAL=0 and calls `dbProcess` before it unlocks.
//! `seqRecord.c::processCallback` (:252-274) runs the last group and the
//! `asyncFinish` `processNextLink` reaches under one lock, and
//! `histogramRecord.c::wdogCallback` (:112-116) zeroes MCNT, stamps and posts
//! under one. The port split each body across separate holds, so a put or a
//! read could land between the decision and the action it owes.
//!
//! Each case reads the decision's own field, then checks the action is
//! already done. The lock set orders the read after the whole body, so no
//! timing makes these pass by chance.

use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

// `Z`/`ZB` count the zeros their OUT target receives.
const DB: &str = r#"
record(longin, "T") {
    field(FLNK, "Z")
}
record(calc, "Z") {
    field(INPA, "T NPP")
    field(INPB, "Z NPP")
    field(CALC, "A=0?B+1:B")
}
record(bo, "B") {
    field(HIGH, "0.2")
    field(OUT, "T.VAL PP")
}
record(longin, "TB") {
    field(FLNK, "ZB")
}
record(calc, "ZB") {
    field(INPA, "TB NPP")
    field(INPB, "ZB NPP")
    field(CALC, "A=0?B+1:B")
}
record(busy, "BSY") {
    field(HIGH, "0.2")
    field(OUT, "TB.VAL PP")
}
record(longin, "ST") { }
record(seq, "S") {
    field(DO0, "5")
    field(LNK0, "ST.VAL PP")
}
record(histogram, "HG") {
    field(SVL,  "2.5")
    field(NELM, "4")
    field(LLIM, "0")
    field(ULIM, "4")
    field(MDEL, "3")
    field(SDEL, "0.1")
}
"#;

/// The momentary records: (record, its OUT target, the target's zero count).
const PULSES: [(&str, &str, &str); 2] = [("B", "T", "Z"), ("BSY", "TB", "ZB")];

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

/// `caput rec 1`: the put, then the process it triggers.
async fn caput_one(db: &PvDatabase, rec: &str) {
    db.put_pv(rec, EpicsValue::Enum(1)).await.unwrap();
    process(db, rec).await;
}

fn read(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn wait_for(db: &PvDatabase, pv: &str, want: f64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while read(db, pv) != want {
        assert!(Instant::now() < deadline, "{pv} never read {want}");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(1)).await;
    }
}

/// The put that lands right after the timer's VAL=0. In C it waits for
/// `myCallbackFunc`'s `dbProcess`, so OUT has carried the 0 by the time the
/// put drives it back to 1; with the zero and its pass in two holds, the put
/// took the pass's place and superseded its token, and OUT never saw the 0.
#[epics_macros_rs::epics_test]
async fn a_put_after_the_high_zero_finds_it_already_driven_out() {
    let db = build().await;
    for (rec, out, zeros) in PULSES {
        caput_one(&db, rec).await;
        assert_eq!(read(&db, out), 1.0, "{rec}: OUT carries the pulse");
        assert_eq!(read(&db, zeros), 0.0, "{rec}: no zero yet");

        wait_for(&db, rec, 0.0).await;
        caput_one(&db, rec).await;
        assert_eq!(
            read(&db, zeros),
            1.0,
            "{rec}: the HIGH timer's zero must reach OUT before a put that follows it"
        );
    }
}

/// The reader half: VAL=0 is never visible before the pass that writes it to
/// OUT.
#[epics_macros_rs::epics_test]
async fn a_reader_that_sees_the_high_zero_sees_out_at_zero() {
    let db = build().await;
    for (rec, out, _) in PULSES {
        caput_one(&db, rec).await;
        wait_for(&db, rec, 0.0).await;
        assert_eq!(
            read(&db, out),
            0.0,
            "{rec}: VAL read 0 while OUT still carried the pulse"
        );
    }
}

/// seq's last group and its `asyncFinish`: the group's write is never visible
/// while the record is still PACT.
#[epics_macros_rs::epics_test]
async fn a_reader_that_sees_the_last_seq_group_sees_pact_clear() {
    let db = build().await;
    process(&db, "S").await;
    wait_for(&db, "ST", 5.0).await;
    assert_eq!(
        read(&db, "S.PACT"),
        0.0,
        "the last group's LNK0 write was visible before asyncFinish cleared PACT"
    );
}

/// The histogram watchdog: its MCNT reset is never visible before the VAL
/// post it is made for.
#[epics_macros_rs::epics_test]
async fn a_reader_that_sees_the_watchdog_reset_sees_its_post() {
    let db = build().await;
    let rec = db.get_record("HG").unwrap();
    let mut val_rx = rec
        .write()
        .add_subscriber("VAL", 1, DbFieldType::ULong, EventMask::VALUE.bits())
        .expect("VAL subscription accepted");

    // One count under MDEL=3: the process itself posts nothing.
    process(&db, "HG").await;
    wait_for(&db, "HG.MCNT", 0.0).await;
    val_rx
        .try_recv()
        .expect("MCNT read 0 before the watchdog posted the count it zeroed");
}
