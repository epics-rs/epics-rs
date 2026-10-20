//! StreamEpics.cc `matchValue`'s `dbPut`/`dbPutField`/`dbProcess` made from
//! inside the record's hold (`DeviceSupport::lends_held`): the status comes
//! back before the protocol goes on, on a bus thread (a), on a bus that
//! answers inline in the dset call (b) and in `@init` (c), so a refused
//! put fails the protocol as in C and a field the put drives is read by the
//! next command. The record's own `dbCommon` fields are put as C puts them.

mod redirect;

use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::ScanType;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use redirect::{ASYNC_REPLIES, REPLIES, REPLIES_ONCE, WRITES, get, ioc, process};

fn f(db: &PvDatabase, pv: &str) -> f64 {
    get(db, pv).to_f64().unwrap()
}

fn s(db: &PvDatabase, pv: &str) -> String {
    get(db, pv).to_string()
}

/// The protocol's second line prints `cnt`, which `t`'s FLNK counted.
async fn put_then_print(bus: &str) {
    REPLIES.lock().push(("H1", "42"));
    let db = ioc(&format!(
        r#"
record(calc, "cnt") {{ field(CALC, "VAL+1") }}
record(ao, "t") {{ field(FLNK, "cnt") }}
record(ai, "a") {{ field(DTYP, "stream") field(INP, "@redirect.proto put_then_print {bus}") }}
"#
    ))
    .await;
    assert_eq!(process(&db, "a", 2).await, ["H1", "1"]);
    assert_eq!(f(&db, "t"), 42.0);
    assert_eq!(f(&db, "a.SEVR"), 0.0);
}

#[test]
fn a_bus_thread_put_answers_before_the_protocol_goes_on() {
    epics_base_rs::runtime::task::test_block_on(put_then_print("R"));
}

#[test]
fn an_inline_answer_puts_from_inside_the_dset_call() {
    epics_base_rs::runtime::task::test_block_on(put_then_print("RI"));
}

#[test]
fn a_refused_put_fails_the_protocol() {
    REPLIES.lock().push(("H2", "3"));
    epics_base_rs::runtime::task::test_block_on(async {
        let db = ioc(r#"
record(ao, "dd") { field(DISP, "1") }
record(ai, "a") { field(DTYP, "stream") field(INP, "@redirect.proto put_refused R") }
"#)
        .await;
        process(&db, "a", 1).await;
        assert_eq!(f(&db, "dd"), 0.0, "dbPutField refuses DISP");
        assert_eq!(f(&db, "a.SEVR"), 3.0);
        assert_eq!(f(&db, "a.STAT"), 12.0, "CALC: the input did not match");
    });
}

#[test]
fn the_record_s_own_dbcommon_field() {
    REPLIES.lock().push(("H3", "own"));
    epics_base_rs::runtime::task::test_block_on(async {
        let db = ioc(r#"
record(ai, "a") { field(DTYP, "stream") field(INP, "@redirect.proto put_own_desc R") }
"#)
        .await;
        process(&db, "a", 1).await;
        assert_eq!(s(&db, "a.DESC"), "own");
        assert_eq!(f(&db, "a.SEVR"), 0.0);
    });
}

/// `tl`'s FLNK reaches the stream record while its protocol runs: the
/// record is active and `tl` was put by dbPutField, so the record is marked
/// RPRO and runs its protocol again once it completes (dbDbLink.c:476-489,
/// recGbl.c recGblFwdLink). In C that repeats for as long as the device
/// answers; the second run here gets no reply, times out and puts nothing.
#[test]
fn a_flnk_chain_back_into_the_stream_record() {
    REPLIES_ONCE.lock().push(("H4", "9"));
    epics_base_rs::runtime::task::test_block_on(async {
        let db = ioc(r#"
record(ao, "tl") { field(FLNK, "sl") }
record(ai, "sl") { field(DTYP, "stream") field(INP, "@redirect.proto put_loop R") }
"#)
        .await;
        process(&db, "sl", 1).await;
        epics_base_rs::runtime::task::sleep(Duration::from_millis(1000)).await;
        assert_eq!(f(&db, "tl"), 9.0);
        let runs = WRITES.lock().iter().filter(|w| *w == "H4").count();
        assert_eq!(runs, 2, "the RPRO rerun, then none");
        assert_eq!(f(&db, "sl.SEVR"), 3.0);
        assert_eq!(f(&db, "sl.STAT"), 10.0, "TIMEOUT: the rerun got no reply");
    });
}

/// `@init`: dbPut into the record's own DESC and into `ti`, whose alarm is
/// cleared; neither is processed. On a bus thread, the `@init` waiter runs
/// the entries, and inline, the dset call.
#[test]
fn init_puts_through_the_hold() {
    REPLIES.lock().push(("H5", "init 17"));
    epics_base_rs::runtime::task::test_block_on(async {
        let db = ioc(r#"
record(calc, "cnt") { field(CALC, "VAL+1") }
record(ai, "ti") { field(FLNK, "cnt") }
record(ao, "e") { field(DTYP, "stream") field(OUT, "@redirect.proto put_init_held R") field(FLNK, "cnt") }
record(ao, "ei") { field(DTYP, "stream") field(OUT, "@redirect.proto put_init_held RI") field(FLNK, "cnt") }
"#)
        .await;
        for r in ["e", "ei"] {
            assert_eq!(s(&db, &format!("{r}.DESC")), "init", "{r}");
        }
        assert_eq!(f(&db, "ti"), 17.0);
        assert_eq!(f(&db, "ti.UDF"), 0.0);
        assert_eq!(f(&db, "ti.SEVR"), 0.0);
        assert_eq!(f(&db, "ti.STAT"), 0.0);
        assert_eq!(f(&db, "cnt"), 0.0, "dbPut processes nothing");
    });
}

fn scan_posts(db: &PvDatabase, rec: &str) -> EventReader {
    let mut rx = db
        .get_record(rec)
        .unwrap()
        .write()
        .add_subscriber("SCAN", 1, DbFieldType::Enum, EventMask::VALUE.bits())
        .unwrap();
    while rx.try_recv().is_ok() {}
    rx
}

fn count(rx: &mut EventReader) -> usize {
    std::iter::from_fn(|| rx.try_recv().ok()).count()
}

fn scan(db: &PvDatabase, rec: &str) -> ScanType {
    db.get_record(rec).unwrap().read().common.scan
}

/// A SCAN put to I/O Intr: `scanAdd` starts the protocol, whose read the
/// bus answers inline and whose put lands before its next command prints
/// `cnt`. `b`'s `@init` failed, so its Add is refused after its protocol
/// ran (no `ioscanpvt`) and it falls back to Passive. Either way SCAN is
/// posted once, after the Add (dbAccess.c:1416-1418).
#[test]
fn a_scan_put_s_add_answers_inline_and_puts() {
    REPLIES.lock().push(("H6", "x"));
    ASYNC_REPLIES.lock().extend([("a", "5"), ("b", "6")]);
    epics_base_rs::runtime::task::test_block_on(async {
        let db = ioc(r#"
record(calc, "cnt") { field(CALC, "VAL+1") }
record(ao, "t") { field(FLNK, "cnt") }
record(ao, "a") { field(DTYP, "stream") field(OUT, "@redirect.proto intr_put RI") }
record(ao, "b") { field(DTYP, "stream") field(OUT, "@redirect.proto intr_bad_init RI") }
"#)
        .await;
        for (rec, val, cnt, want) in [
            ("a", 5.0, "1", ScanType::IoIntr),
            ("b", 6.0, "2", ScanType::Passive),
        ] {
            let mut posts = scan_posts(&db, rec);
            let before = WRITES.lock().len();
            db.put_record_field_from_ca_no_notify(
                rec,
                "SCAN",
                EpicsValue::String("I/O Intr".into()),
            )
            .await
            .unwrap();
            assert_eq!(f(&db, "t"), val, "{rec}");
            assert_eq!(WRITES.lock()[before..], [cnt.to_string()], "{rec}");
            assert_eq!(scan(&db, rec), want, "{rec}");
            assert_eq!(count(&mut posts), 1, "{rec}: one SCAN post");
        }
    });
}
