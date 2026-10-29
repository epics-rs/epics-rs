//! `AsyncDbHandle::name_to_addr` / `db_get` are C `dbNameToAddr` and `dbGet`
//! as sscan calls them from inside its own record (`sscanRecord.c:2335`,
//! `:2752`, `:3342`, `:3749`): `no_elements` is the `cvt_dbaddr` capacity,
//! the value comes back in the requested type cut to the request, the
//! metadata rides along, and a call from inside a record's own processing
//! reads that record from itself and waits for no lock set, as C's lock-free
//! `dbGet` does.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::{DbFieldType, DbfCode, EpicsValue};

const DB: &str = r#"
record(ai, "AI") {
    field(DESC, "beam")
    field(VAL, "1.5")
    field(EGU, "mm")
    field(PREC, "3")
    field(HOPR, "10")
    field(LOPR, "-10")
}
record(ao, "AO") {
    field(DRVH, "9")
    field(DRVL, "-9")
    field(HOPR, "10")
    field(LOPR, "-10")
}
record(waveform, "W") {
    field(NELM, "8")
    field(FTVL, "DOUBLE")
}
record(stringin, "S") {
    field(VAL, "abc")
}
record(mbbi, "M") {
    field(ZRST, "zero")
    field(ONST, "one")
    field(VAL, "1")
}
record(calc, "C") {
    field(INPA, "H.VAL")
    field(CALC, "A")
}
record(calc, "H") {
    field(INPA, "AI.VAL")
    field(CALC, "A")
}
record(ai, "AOR") {
    field(INP, "AO.VAL")
}
record(mbbo, "MO") {}
"#;

async fn ioc() -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db.put_pv("W.VAL", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0].into()))
        .await
        .unwrap();
    db
}

#[epics_macros_rs::epics_test]
async fn name_to_addr_answers_the_cvt_dbaddr_capacity() {
    let db = ioc().await;
    let h = db.async_handle();
    assert_eq!(h.name_to_addr("AI.VAL"), Some(1));
    assert_eq!(h.name_to_addr("AI"), Some(1));
    // NELM, not the NORD of 3 a get returns.
    assert_eq!(h.name_to_addr("W.VAL"), Some(8));
    assert_eq!(h.name_to_addr("W.NORD"), Some(1));
    assert_eq!(h.name_to_addr("NOSUCH.VAL"), None);
    assert_eq!(h.name_to_addr("AI.NOSUCH"), None);
}

#[epics_macros_rs::epics_test]
async fn db_get_serves_the_value_and_its_metadata() {
    let db = ioc().await;
    let h = db.async_handle();
    let snap = h.db_get("AI.VAL", DbFieldType::Double, 1, None).unwrap();
    assert_eq!(snap.value, EpicsValue::Double(1.5));
    assert_eq!(snap.units().map(|u| u.to_string()), Some("mm".into()));
    assert_eq!(snap.precision(), Some(3));
    assert_eq!(snap.graphic_limits(), Some((-10.0, 10.0)));

    let snap = h.db_get("AI.VAL", DbFieldType::Float, 1, None).unwrap();
    assert_eq!(snap.value, EpicsValue::Float(1.5));

    let snap = h.db_get("AO.VAL", DbFieldType::Double, 1, None).unwrap();
    assert_eq!(snap.control_limits(), Some((-9.0, 9.0)));
}

#[epics_macros_rs::epics_test]
async fn db_get_returns_at_most_the_request_and_at_most_what_is_there() {
    let db = ioc().await;
    let h = db.async_handle();
    let all = h.db_get("W.VAL", DbFieldType::Double, 8, None).unwrap();
    assert_eq!(
        all.value,
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0].into())
    );
    let two = h.db_get("W.VAL", DbFieldType::Float, 2, None).unwrap();
    assert_eq!(two.value, EpicsValue::FloatArray(vec![1.0, 2.0].into()));
}

#[epics_macros_rs::epics_test]
async fn db_get_fails_where_c_dbget_does() {
    let db = ioc().await;
    let h = db.async_handle();
    assert!(
        h.db_get("NOSUCH.VAL", DbFieldType::Double, 1, None)
            .is_none()
    );
    // `cvt_st_d` on unparseable text is a non-zero status.
    assert!(h.db_get("S.VAL", DbFieldType::Double, 1, None).is_none());
}

/// `DBR_STRING` renders an ENUM through the record's `get_enum_str` and a
/// DOUBLE at its precision (`getEnumString`, `getDoubleString`,
/// `dbConvert.c:833`, `:772`), not as the bare number.
#[epics_macros_rs::epics_test]
async fn db_get_as_a_string_renders_as_the_record_does() {
    let db = ioc().await;
    let h = db.async_handle();
    let text = |name| h.db_get(name, DbFieldType::String, 1, None).unwrap().value;
    assert_eq!(text("M.VAL"), EpicsValue::String("one".into()));
    assert_eq!(text("AI.VAL"), EpicsValue::String("1.500".into()));
}

/// What `db_get` serves for `name` with no hold, to compare a held answer to.
fn unheld(db: &PvDatabase, name: &str) -> Snapshot {
    db.async_handle()
        .db_get(name, DbFieldType::Double, 1, None)
        .unwrap()
}

/// Units, precision, graphic and control limits: the four `options` slots.
type Meta = (
    Option<String>,
    Option<i16>,
    Option<(f64, f64)>,
    Option<(f64, f64)>,
);

fn metadata(s: &Snapshot) -> Meta {
    (
        s.units().map(|u| u.to_string()),
        s.precision(),
        s.graphic_limits(),
        s.control_limits(),
    )
}

/// `db_get` of `name` from inside `own`'s processing, as `own` calls it.
fn from_inside(db: &PvDatabase, own: &str, name: &str, ty: DbFieldType) -> Option<Snapshot> {
    let h = db.async_handle();
    h.with_own(own, |rec, own| h.db_get(name, ty, 1, Some(own.of(&*rec))))
        .flatten()
}

#[epics_macros_rs::epics_test]
async fn db_get_from_inside_a_record_serves_its_own_metadata() {
    let db = ioc().await;
    let want = unheld(&db, "AI.VAL");
    let got = from_inside(&db, "AI", "AI.VAL", DbFieldType::Double)
        .expect("the record's own field reads");
    assert_eq!(got.value, EpicsValue::Double(1.5));
    assert_eq!(metadata(&got), metadata(&want));
    assert_eq!(got.units().map(|u| u.to_string()), Some("mm".into()));
    let want = unheld(&db, "AO.VAL");
    let got = from_inside(&db, "AO", "AO.VAL", DbFieldType::Double).expect("AO.VAL reads");
    assert_eq!(metadata(&got), metadata(&want));
    assert_eq!(got.control_limits(), Some((-9.0, 9.0)));
}

/// C's `dbGet` reads the caller's dbCommon and PACT through `prec` as it
/// reads the record's own fields.
#[epics_macros_rs::epics_test]
async fn db_get_from_inside_a_record_reads_its_dbcommon() {
    let db = ioc().await;
    let h = db.async_handle();
    h.with_instance("AI", |_, common| common.sevr = AlarmSeverity::Major);
    db.get_record("AI").unwrap().read().enter_pact();
    let (sevr, desc, pact) = h
        .with_own("AI", |rec, own| {
            let own = Some(own.of(&*rec));
            (
                h.db_get("AI.SEVR", DbFieldType::Short, 1, own),
                h.db_get("AI.DESC", DbFieldType::String, 1, own),
                h.db_get("AI.PACT", DbFieldType::Short, 1, own),
            )
        })
        .expect("AI is loaded");
    let sevr = sevr.expect("AI.SEVR reads");
    assert_eq!(sevr.value, EpicsValue::Short(2));
    assert_eq!(sevr.alarm.severity, AlarmSeverity::Major as u16);
    assert_eq!(
        desc.map(|s| s.value),
        Some(EpicsValue::String("beam".into()))
    );
    assert_eq!(pact.map(|s| s.value), Some(EpicsValue::Short(1)));
}

#[epics_macros_rs::epics_test]
async fn db_get_from_inside_a_record_sees_its_own_metadata_writes() {
    let db = ioc().await;
    db.put_pv("AI.EGU", EpicsValue::String("um".into()))
        .await
        .unwrap();
    db.put_pv("AI.PREC", EpicsValue::Short(5)).await.unwrap();
    let got = from_inside(&db, "AI", "AI.VAL", DbFieldType::Double).expect("AI.VAL reads");
    assert_eq!(got.units().map(|u| u.to_string()), Some("um".into()));
    assert_eq!(got.precision(), Some(5));
}

/// Written in place, inside the same hold: the read is of the fields.
#[epics_macros_rs::epics_test]
async fn db_get_from_inside_a_record_serves_metadata_it_wrote_in_place() {
    let db = ioc().await;
    let h = db.async_handle();
    let got = h.with_own("AI", |rec, own| {
        rec.put_field("EGU", EpicsValue::String("um".into()))
            .unwrap();
        rec.put_field("PREC", EpicsValue::Short(5)).unwrap();
        rec.put_field("HOPR", EpicsValue::Double(20.0)).unwrap();
        rec.put_field("LOPR", EpicsValue::Double(-20.0)).unwrap();
        h.db_get("AI.VAL", DbFieldType::Double, 1, Some(own.of(&*rec)))
    });
    let got = got.flatten().expect("AI.VAL reads");
    assert_eq!(got.units().map(|u| u.to_string()), Some("um".into()));
    assert_eq!(got.precision(), Some(5));
    assert_eq!(got.graphic_limits(), Some((-20.0, 20.0)));
}

#[epics_macros_rs::epics_test]
async fn db_get_from_inside_a_record_resolves_a_link_that_reaches_it() {
    let db = ioc().await;
    // `H.A` is backed by `H.INPA` -> `AI.VAL`: C's `dbGetUnits` through the
    // link reads AI, the record the caller is inside.
    let want = unheld(&db, "H.A");
    assert_eq!(want.units().map(|u| u.to_string()), Some("mm".into()));
    let got = from_inside(&db, "AI", "H.A", DbFieldType::Double).expect("H.A reads");
    assert_eq!(metadata(&got), metadata(&want));
}

#[epics_macros_rs::epics_test]
async fn db_get_from_inside_a_record_does_not_lock_it_again() {
    let db = ioc().await;
    let h = db.async_handle();
    let got = h.with_own("H", |rec, own| {
        let own = Some(own.of(&*rec));
        let mine = h
            .db_get("H.VAL", DbFieldType::Double, 1, own)
            .map(|s| s.value);
        // A link-backed field of another record whose link names this one:
        // resolving it locks H unless it reads H from itself.
        let other = h
            .db_get("C.A", DbFieldType::Double, 1, own)
            .map(|s| s.value);
        (mine, other, h.name_to_addr("H.VAL"))
    });
    let (mine, other, nelem) = got.expect("with_own ran");
    assert!(mine.is_some(), "the record's own field reads");
    assert_eq!(other, Some(EpicsValue::Double(0.0)));
    assert_eq!(nelem, Some(1));
}

/// C's `dbGet` reads a record another thread holds; here its holder reads
/// it as it lets the set go, so the read sees what the hold wrote and a field
/// never named before reads too.
#[epics_macros_rs::epics_test]
async fn db_get_of_a_record_held_elsewhere_reads_it_at_the_holders_release() {
    let db = ioc().await;
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let holder = {
        let h = db.async_handle();
        std::thread::spawn(move || {
            h.with_record("AI", |rec| {
                held_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                rec.put_field("VAL", EpicsValue::Double(7.5)).unwrap();
            });
        })
    };
    held_rx.recv().unwrap();
    let reader = {
        let h = db.async_handle();
        std::thread::spawn(move || {
            (
                h.db_get("AI.VAL", DbFieldType::Double, 1, None),
                h.db_get("AI.HOPR", DbFieldType::Double, 1, None),
            )
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(100));
    go_tx.send(()).unwrap();
    holder.join().unwrap();
    let (val, hopr) = reader.join().unwrap();
    let val = val.expect("read at the release");
    assert_eq!(val.value, EpicsValue::Double(7.5));
    assert_eq!(val.units().map(|u| u.to_string()), Some("mm".into()));
    assert_eq!(hopr.map(|s| s.value), Some(EpicsValue::Double(10.0)));
}

/// Two records each reading the other from inside its own hold: each
/// serves the read the other owes it while it waits for its own.
#[epics_macros_rs::epics_test]
async fn two_records_reading_each_other_from_their_own_holds_both_read() {
    let db = ioc().await;
    let both = Arc::new(std::sync::Barrier::new(2));
    let read = |me: &'static str, other: &'static str| {
        let (h, both) = (db.async_handle(), both.clone());
        std::thread::spawn(move || {
            h.with_own(me, |rec, own| {
                both.wait();
                h.db_get(other, DbFieldType::Double, 1, Some(own.of(&*rec)))
                    .map(|s| s.value)
            })
            .flatten()
        })
    };
    let a = read("AI", "AO.HOPR");
    let b = read("AO", "AI.HOPR");
    assert_eq!(a.join().unwrap(), Some(EpicsValue::Double(10.0)));
    assert_eq!(b.join().unwrap(), Some(EpicsValue::Double(10.0)));
}

/// The message of a thread's panic.
fn panic_text(e: Box<dyn std::any::Any + Send>) -> String {
    e.downcast_ref::<String>()
        .cloned()
        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default()
}

/// A record this thread holds for writing reads only through the view its
/// borrower lends; read without it, it would read as nothing where C's
/// `dbGet` reads the live field, so the read is refused at the call.
#[epics_macros_rs::epics_test]
async fn db_get_of_a_record_this_thread_borrows_without_its_view_is_refused() {
    let db = ioc().await;
    let h = db.async_handle();
    let got = std::thread::spawn(move || {
        h.with_instance("AI", |_, _| {
            h.db_get("AI.VAL", DbFieldType::Double, 1, None)
        })
    })
    .join();
    let msg = panic_text(got.expect_err("the read is refused, not answered"));
    assert!(msg.contains("AI"), "{msg}");
}

/// Two holders each waiting for a member of the other's set that the other
/// borrows without lending a view: neither could serve the other, and both
/// waited for good. The holder serving such a read refuses it, and goes on.
#[epics_macros_rs::epics_test]
async fn holders_borrowing_set_mates_without_a_view_do_not_wait_on_each_other() {
    let db = ioc().await;
    let both = Arc::new(std::sync::Barrier::new(2));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let read = |me: &'static str, mate: &'static str, other: &'static str| {
        let (h, both, done) = (db.async_handle(), both.clone(), done_tx.clone());
        std::thread::spawn(move || {
            let got = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                h.with_own(me, |rec, own| {
                    h.with_instance(mate, |_, _| {
                        both.wait();
                        h.db_get(other, DbFieldType::Double, 1, Some(own.of(&*rec)))
                    })
                })
            }));
            done.send((other, got.map_err(panic_text))).unwrap();
        })
    };
    // H is AI's set-mate, AOR AO's.
    read("AI", "H", "AOR.HOPR");
    read("AO", "AOR", "H.HOPR");
    let wait = fact_budget::FACT_BUDGET;
    let got: Vec<_> = (0..2)
        .map(|_| done_rx.recv_timeout(wait).expect("both holders finish"))
        .collect();
    // Refused while the holder borrows it, or read live once it does not.
    for (other, g) in got {
        let read = g.expect("no holder panics").flatten().flatten();
        let live = db.get_pv(other).unwrap();
        assert!(
            read.as_ref().is_none_or(|s| s.value == live),
            "{other} {read:?}"
        );
    }
}

/// Two holders that lend no view, each reading the other's record: a waiter
/// serves the reads owed to every set it holds, not only its own record's,
/// or each waits for the other's release for good.
#[epics_macros_rs::epics_test]
async fn two_holders_reading_each_other_without_a_view_both_read() {
    let db = ioc().await;
    let both = Arc::new(std::sync::Barrier::new(2));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let read = |me: &'static str, other: &'static str| {
        let (h, both, done) = (db.async_handle(), both.clone(), done_tx.clone());
        std::thread::spawn(move || {
            let got = h.with_held(me, |_| {
                both.wait();
                h.db_get(other, DbFieldType::Double, 1, None)
                    .map(|s| s.value)
            });
            done.send((me, got.flatten())).unwrap();
        })
    };
    read("AI", "AO.HOPR");
    read("AO", "AI.HOPR");
    let wait = fact_budget::FACT_BUDGET;
    for _ in 0..2 {
        let (me, got) = done_rx.recv_timeout(wait).expect("both holders read");
        assert_eq!(got, Some(EpicsValue::Double(10.0)), "{me}");
    }
}

/// A read owed to a holder's set before that holder began to wait is served
/// from its wait too: AO's holder waits on AI, whose holder waits for M,
/// whose holder waits on its read of AO.
#[epics_macros_rs::epics_test]
async fn a_holder_serves_a_read_owed_before_it_waits() {
    let db = ioc().await;
    let all = Arc::new(std::sync::Barrier::new(3));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let ms = std::time::Duration::from_millis;
    {
        let (h, all, done) = (db.async_handle(), all.clone(), done_tx.clone());
        std::thread::spawn(move || {
            h.with_held("AI", |_| {
                all.wait();
                std::thread::sleep(ms(50));
                h.with_record("M", |_| ());
            });
            done.send(("AI", Some(EpicsValue::Double(10.0)))).unwrap();
        });
    }
    let read = |me: &'static str, other: &'static str, after| {
        let (h, all, done) = (db.async_handle(), all.clone(), done_tx.clone());
        std::thread::spawn(move || {
            let got = h.with_held(me, |_| {
                all.wait();
                std::thread::sleep(ms(after));
                h.db_get(other, DbFieldType::Double, 1, None)
                    .map(|s| s.value)
            });
            done.send((me, got.flatten())).unwrap();
        })
    };
    read("M", "AO.HOPR", 20);
    read("AO", "AI.HOPR", 100);
    let wait = fact_budget::FACT_BUDGET;
    for _ in 0..3 {
        let (me, got) = done_rx.recv_timeout(wait).expect("all three finish");
        assert_eq!(got, Some(EpicsValue::Double(10.0)), "{me}");
    }
}

/// One thread holds AI and takes AO's set; the other holds AO and reads AI.
/// C's `dbGet` reads AI unlocked, so both finish. Here AI's holder waits on
/// a set the reader holds and can serve nothing: the read is refused.
#[epics_macros_rs::epics_test]
async fn a_read_whose_holder_waits_on_the_reader_is_refused() {
    let db = ioc().await;
    let both = Arc::new(std::sync::Barrier::new(2));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    {
        let (h, both, done) = (db.async_handle(), both.clone(), done_tx.clone());
        std::thread::spawn(move || {
            h.with_own("AI", |_, _| {
                both.wait();
                std::thread::sleep(std::time::Duration::from_millis(50));
                h.with_record("AO", |_| ());
            });
            done.send(("AI", None)).unwrap();
        });
    }
    {
        let (h, both, done) = (db.async_handle(), both.clone(), done_tx.clone());
        std::thread::spawn(move || {
            let got = h.with_own("AO", |rec, own| {
                both.wait();
                std::thread::sleep(std::time::Duration::from_millis(100));
                h.db_get("AI.VAL", DbFieldType::Double, 1, Some(own.of(&*rec)))
                    .map(|s| s.value)
            });
            done.send(("AO", got.flatten())).unwrap();
        });
    }
    let wait = fact_budget::FACT_BUDGET;
    for _ in 0..2 {
        let (me, got) = done_rx.recv_timeout(wait).expect("both threads finish");
        assert_eq!(got, None, "{me}");
    }
}

/// The holder of AI waits for M, whose holder waits for AO, which the reader
/// of AI holds: a chain of takers ends at the reader, and the read fails.
/// C's `dbGet` would read AI's 10 unlocked.
#[epics_macros_rs::epics_test]
async fn a_read_whose_holder_waits_behind_another_on_the_reader_is_refused() {
    let db = ioc().await;
    let all = Arc::new(std::sync::Barrier::new(3));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let ms = std::time::Duration::from_millis;
    let take = |me: &'static str, then: &'static str, after| {
        let (h, all, done) = (db.async_handle(), all.clone(), done_tx.clone());
        std::thread::spawn(move || {
            h.with_held(me, |_| {
                all.wait();
                std::thread::sleep(ms(after));
                h.with_record(then, |_| ());
            });
            done.send((me, None)).unwrap();
        })
    };
    take("AI", "M", 30);
    take("M", "AO", 60);
    {
        let (h, all, done) = (db.async_handle(), all.clone(), done_tx.clone());
        std::thread::spawn(move || {
            let got = h.with_held("AO", |_| {
                all.wait();
                std::thread::sleep(ms(100));
                h.db_get("AI.HOPR", DbFieldType::Double, 1, None)
                    .map(|s| s.value)
            });
            done.send(("AO", got.flatten())).unwrap();
        });
    }
    let wait = fact_budget::FACT_BUDGET;
    for _ in 0..3 {
        let (me, got) = done_rx.recv_timeout(wait).expect("all three finish");
        assert_eq!(got, None, "{me}");
    }
}

/// `dbNameToAddr`'s `field_type` for `name` while another thread holds its
/// record.
fn typed_while_held(db: &PvDatabase, name: &'static str) -> DbfCode {
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let h = db.async_handle();
    let record = name.split('.').next().unwrap();
    let holder = std::thread::spawn(move || {
        h.with_held(record, |_| {
            held_tx.send(()).unwrap();
            done_rx.recv().unwrap();
        });
    });
    held_rx.recv().unwrap();
    let addr = db.async_handle().name_to_dbaddr(name, None);
    done_tx.send(()).unwrap();
    holder.join().unwrap();
    addr.unwrap().field_type
}

/// C `dbNameToAddr` takes a runtime-typed field's `field_type` from
/// `cvt_dbaddr`, which reads `prec->ftvl` with no lock
/// (`waveformRecord.c:184`): a record another thread holds is typed too.
#[epics_macros_rs::epics_test]
async fn name_to_dbaddr_types_a_field_of_a_record_held_elsewhere() {
    let db = ioc().await;
    assert_eq!(typed_while_held(&db, "W.VAL"), DbfCode::Double);
}

/// An mbbo's VAL is `DBF_USHORT` with no state defined and `DBF_ENUM` with
/// one (`mbboRecord.c` `cvt_dbaddr`, from `sdef`): a state put retypes it for
/// a reader that finds the record held.
#[epics_macros_rs::epics_test]
async fn name_to_dbaddr_types_an_mbbo_val_held_elsewhere_by_its_states() {
    let db = ioc().await;
    assert_eq!(typed_while_held(&db, "MO.VAL"), DbfCode::UShort);
    db.put_pv("MO.ZRST", EpicsValue::String("zero".into()))
        .await
        .unwrap();
    assert_eq!(typed_while_held(&db, "MO.VAL"), DbfCode::Enum);
}
