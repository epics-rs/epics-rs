//! calc and calcout `monitor()` decide an input's post against its LA..LU
//! copy (`calcRecord.c:418`, `calcoutRecord.c:681`), not against what a put
//! already posted, so a put input posts on the put and again on the cycle.
//! Oracle — softIoc 7.0.10.1-DEV, CA and PVA alike:
//!
//! ```text
//! record(ai, "SRC") { field(VAL, "3") }
//! record(calc,    "CA") { field(CALC, "B") field(B, "5") field(HIGH, "1") field(HSV, "MINOR") }
//! record(calcout, "CO") { field(CALC, "B") field(B, "5") field(HIGH, "1") field(HSV, "MINOR") }
//! record(calcout, "CN") { field(CALC, "A") field(INPA, "5") }
//!
//! caput CA.PROC 1               (settles HIGH MINOR)
//! caput CA.B 6        ->  CA.B 6  CA.VAL 6  CA.B 6
//! caput CA.PROC 1     ->  nothing
//! caput CN.INPA SRC   ->  CN.A 5
//! caput CN.PROC 1     ->  CN.VAL 3  CN.A 3
//! ```
//!
//! CO streams as CA.

use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

fn subscribe(db: &PvDatabase, rec: &str, field: &str) -> EventReader {
    db.get_record(rec)
        .unwrap()
        .write()
        .add_subscriber(
            field,
            1,
            DbFieldType::Double,
            (EventMask::VALUE | EventMask::ALARM).bits(),
        )
        .expect("subscriber")
}

fn drain(rx: &mut EventReader) -> Vec<EpicsValue> {
    let mut got = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        got.push(ev.value().as_value().cloned().unwrap());
    }
    got
}

async fn caput(db: &PvDatabase, rec: &str, field: &str, value: &str) {
    db.put_record_field_from_ca_no_notify(rec, field, EpicsValue::String(value.into()))
        .await
        .unwrap();
    epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
}

fn configure(rec: &mut dyn Record) {
    for (field, value) in [
        ("CALC", EpicsValue::String("B".into())),
        ("B", EpicsValue::Double(5.0)),
    ] {
        rec.put_field(field, value).unwrap();
    }
}

async fn a_put_input_posts_on_the_put_and_on_the_cycle(db: PvDatabase, rec: &str) {
    db.ioc_init().await;
    caput(&db, rec, "HIGH", "1").await;
    caput(&db, rec, "HSV", "MINOR").await;
    caput(&db, rec, "PROC", "1").await;
    let mut b = subscribe(&db, rec, "B");

    caput(&db, rec, "B", "6").await;
    let six = EpicsValue::Double(6.0);
    assert_eq!(drain(&mut b), vec![six.clone(), six], "caput {rec}.B 6");

    caput(&db, rec, "PROC", "1").await;
    assert_eq!(
        drain(&mut b),
        Vec::new(),
        "caput {rec}.PROC 1, nothing moved"
    );
}

#[epics_macros_rs::epics_test]
async fn calc_posts_a_put_input_on_the_put_and_on_the_cycle() {
    let db = PvDatabase::new();
    let mut ca = CalcRecord::default();
    configure(&mut ca);
    db.add_record("CA", Box::new(ca)).await.unwrap();
    a_put_input_posts_on_the_put_and_on_the_cycle(db, "CA").await;
}

#[epics_macros_rs::epics_test]
async fn calcout_posts_a_put_input_on_the_put_and_on_the_cycle() {
    let db = PvDatabase::new();
    let mut co = CalcoutRecord::default();
    configure(&mut co);
    db.add_record("CO", Box::new(co)).await.unwrap();
    a_put_input_posts_on_the_put_and_on_the_cycle(db, "CO").await;
}

#[epics_macros_rs::epics_test]
async fn a_repointed_input_posts_on_the_udf_clearing_cycle() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AiRecord::new(3.0)))
        .await
        .unwrap();
    let mut cn = CalcoutRecord::default();
    cn.put_field("CALC", EpicsValue::String("A".into()))
        .unwrap();
    cn.put_field("INPA", EpicsValue::String("5".into()))
        .unwrap();
    db.add_record("CN", Box::new(cn)).await.unwrap();
    db.ioc_init().await;
    let mut a = subscribe(&db, "CN", "A");

    caput(&db, "CN", "INPA", "SRC").await;
    assert_eq!(
        drain(&mut a),
        vec![EpicsValue::Double(5.0)],
        "caput CN.INPA SRC"
    );
    caput(&db, "CN", "PROC", "1").await;
    assert_eq!(
        drain(&mut a),
        vec![EpicsValue::Double(3.0)],
        "caput CN.PROC 1"
    );
}
