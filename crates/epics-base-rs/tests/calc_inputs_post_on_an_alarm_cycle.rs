//! calc and calcout `monitor()` post every input A..U on a cycle whose alarm
//! moved, changed or not (`calcRecord.c:418-420`, `calcoutRecord.c:681-682`).
//! Oracle — softIoc 7.0.10.1-DEV:
//!
//! ```text
//! record(calcout, "CO") { field(CALC, "A+B") field(HIGH, "5") field(HSV, "MINOR") field(A, "1") field(B, "1") }
//! record(calc,    "CA") { field(CALC, "A+B") field(HIGH, "5") field(HSV, "MINOR") field(A, "1") field(B, "1") }
//!
//! caput CO.PROC 1; caput CA.PROC 1; camonitor CO.A CA.A; caput CO.B 5; caput CA.B 5
//! CO.A  1
//! CA.A  1
//! CO.A  1 HIGH MINOR
//! CA.A  1 HIGH MINOR
//! ```

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

fn configure(rec: &mut dyn Record) {
    for (field, value) in [
        ("CALC", EpicsValue::String("A+B".into())),
        ("A", EpicsValue::Double(1.0)),
        ("B", EpicsValue::Double(1.0)),
    ] {
        rec.put_field(field, value).unwrap();
    }
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

async fn an_unchanged_input_posts_when_the_alarm_moves(db: PvDatabase, rec: &str) {
    db.put_pv(&format!("{rec}.HIGH"), EpicsValue::Double(5.0))
        .await
        .unwrap();
    db.put_pv(&format!("{rec}.HSV"), EpicsValue::String("MINOR".into()))
        .await
        .unwrap();
    process(&db, rec).await;
    let mut rx = {
        let inst = db.get_record(rec).unwrap();
        let mut g = inst.write();
        g.add_subscriber(
            "A",
            1,
            DbFieldType::Double,
            (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits(),
        )
        .expect("subscription accepted")
    };
    db.get_record(rec)
        .unwrap()
        .write()
        .record
        .put_field("B", EpicsValue::Double(5.0))
        .unwrap();
    process(&db, rec).await;

    let mut masks = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        masks.push(ev.mask);
    }
    assert_eq!(
        masks.len(),
        1,
        "{rec}.A posts once on the HIGH MINOR cycle: {masks:?}"
    );
    assert!(
        masks[0].contains(EventMask::ALARM),
        "{rec}.A carries the alarm class: {masks:?}"
    );
}

#[epics_macros_rs::epics_test]
async fn calcout_posts_an_unchanged_input_on_an_alarm_cycle() {
    let db = PvDatabase::new();
    let mut co = CalcoutRecord::default();
    configure(&mut co);
    db.add_record("CO", Box::new(co)).await.unwrap();
    an_unchanged_input_posts_when_the_alarm_moves(db, "CO").await;
}

#[epics_macros_rs::epics_test]
async fn calc_posts_an_unchanged_input_on_an_alarm_cycle() {
    let db = PvDatabase::new();
    let mut ca = CalcRecord::default();
    configure(&mut ca);
    db.add_record("CA", Box::new(ca)).await.unwrap();
    an_unchanged_input_posts_when_the_alarm_moves(db, "CA").await;
}
