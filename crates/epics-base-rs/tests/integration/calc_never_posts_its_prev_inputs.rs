//! calc `monitor()` writes LA..LU without posting them (`calcRecord.c:415-423`).
//! Oracle — softIoc 7.0.10.1-DEV, `camonitor -m v|a|l X X.A X.B X.LA X.LB`:
//!
//! ```text
//! record(calc, "X") { field(CALC, "A+B") }
//!
//! caput X.A 1; X.A 2; X.B 3   ->  LA and LB post nothing in any class
//! ```

use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

#[epics_macros_rs::epics_test]
async fn calc_writes_la_without_posting_it() {
    let db = PvDatabase::new();
    let mut rec = CalcRecord::default();
    rec.put_field("CALC", EpicsValue::String("A+B".into()))
        .unwrap();
    db.add_record("X", Box::new(rec)).await.unwrap();
    db.ioc_init().await;
    let mut readers = Vec::new();
    for (sid, field) in ["LA", "LB"].into_iter().enumerate() {
        let mask = EventMask::VALUE | EventMask::ALARM | EventMask::LOG;
        readers.push(
            db.get_record("X")
                .unwrap()
                .write()
                .add_subscriber(field, sid as u32 + 1, DbFieldType::Double, mask.bits())
                .unwrap(),
        );
    }
    for (field, value) in [("A", "1"), ("A", "2"), ("B", "3")] {
        db.put_record_field_from_ca_no_notify("X", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
        epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
    }
    for (field, rx) in ["LA", "LB"].into_iter().zip(&mut readers) {
        let mut got = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            got.push(ev.value().as_value().cloned().unwrap());
        }
        assert_eq!(got, Vec::new(), "X.{field}");
    }
}
