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
use epics_base_rs::types::{DbFieldType, EpicsValue};

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

#[epics_macros_rs::epics_test]
async fn db_get_of_a_record_held_elsewhere_serves_its_last_read() {
    let db = ioc().await;
    let h = db.async_handle();
    assert_eq!(h.name_to_addr("AI.VAL"), Some(1));
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let holder = {
        let h = db.async_handle();
        std::thread::spawn(move || {
            h.with_record("AI", |_| {
                held_tx.send(()).unwrap();
                done_rx.recv().unwrap();
            });
        })
    };
    held_rx.recv().unwrap();
    // The read `name_to_addr` took, not a wait for the holder.
    let got = h.db_get("AI.VAL", DbFieldType::Double, 1, None);
    // A field never read before has nothing to serve while AI is held.
    let unread = h.db_get("AI.HOPR", DbFieldType::Double, 1, None);
    done_tx.send(()).unwrap();
    holder.join().unwrap();
    let got = got.expect("the last read is served");
    assert_eq!(got.value, EpicsValue::Double(1.5));
    assert_eq!(got.units().map(|u| u.to_string()), Some("mm".into()));
    assert!(unread.is_none());
}

/// C's `dbGet` reads through the `DBADDR` `dbNameToAddr` gave, so a field a
/// device resolved with `name_to_dbaddr` (stream's `%(record.FIELD)`) is read
/// while its record is held elsewhere, as one `name_to_addr` resolved is.
#[epics_macros_rs::epics_test]
async fn db_get_of_a_dbaddr_held_elsewhere_serves_its_last_read() {
    let db = ioc().await;
    let h = db.async_handle();
    let addr = h.name_to_dbaddr("AI.HOPR", None).expect("resolves");
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let holder = {
        let h = db.async_handle();
        std::thread::spawn(move || {
            h.with_record("AI", |_| {
                held_tx.send(()).unwrap();
                done_rx.recv().unwrap();
            });
        })
    };
    held_rx.recv().unwrap();
    let name = format!("{}.{}", addr.record.to_str_lossy(), addr.field);
    let got = h.db_get(&name, DbFieldType::Double, 1, None);
    done_tx.send(()).unwrap();
    holder.join().unwrap();
    assert_eq!(got.map(|s| s.value), Some(EpicsValue::Double(10.0)));
}
