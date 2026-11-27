//! A runtime put to a calcout link posts what C's `special()` posts:
//! the link's value field and its status field reading Constant, whatever
//! the new target, and nothing else (calcoutRecord.c:366-405). The status is
//! Constant because `dbPutFieldLink` calls `dbPutSpecial(paddr, 1)` before
//! `dbAddLink` sets the `lset` (dbAccess.c:1181-1208).
//!
//! Oracle — softIoc R7.0.10 (CA, camonitor) and softIocPVX on pvxs 1.5.3
//! (PVA, pvxmonitor), identical event sequences, on
//!
//! ```text
//! record(ai, "SRC") { field(VAL, "3") }
//! record(calcout, "CO") { field(CALC, "A") field(INPA, "5") }
//!
//! caput CO.INPA SRC   ->  CO.A 5     CO.INAV Constant
//! caput CO.INPA 7     ->  CO.A 5     CO.INAV Constant   (A not reloaded)
//! caput CO.INPA ""    ->  CO.A 5     CO.INAV Constant
//! caput CO.OUT SRC    ->  CO.OVAL 0  CO.OUTV Constant
//! caput CO.PROC 1     ->  CO.A 5     CO.OVAL 5          (no INAV/INBV/OUTV)
//! ```
//!
//! CO.INBV posts nothing after the connect value.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const CONSTANT: u16 = 3;
const FIELDS: [&str; 5] = ["A", "INAV", "INBV", "OUTV", "OVAL"];

fn drain(rx: &mut EventReader) -> Vec<EpicsValue> {
    let mut got = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        got.push(ev.value().as_value().cloned().unwrap());
    }
    got
}

#[epics_macros_rs::epics_test]
async fn calcout_link_puts_post_as_softioc_r7_0_10() {
    let db = PvDatabase::new();
    db.add_record("SRC", Box::new(AiRecord::new(3.0)))
        .await
        .unwrap();
    let mut co = CalcoutRecord::default();
    co.put_field("CALC", EpicsValue::String("A".into()))
        .unwrap();
    co.put_field("INPA", EpicsValue::String("5".into()))
        .unwrap();
    db.add_record("CO", Box::new(co)).await.unwrap();
    db.ioc_init().await;

    let rec = db.get_record("CO").unwrap();
    let mut rx: Vec<EventReader> = FIELDS
        .iter()
        .map(|f| {
            let ft = if f.ends_with('V') {
                DbFieldType::Enum
            } else {
                DbFieldType::Double
            };
            rec.write()
                .add_subscriber(f, 1, ft, (EventMask::VALUE | EventMask::ALARM).bits())
                .expect("subscriber")
        })
        .collect();

    let a5 = || vec![EpicsValue::Double(5.0)];
    let con = || vec![EpicsValue::Enum(CONSTANT)];
    let none = Vec::<EpicsValue>::new;
    let steps: [(&str, &str, [Vec<EpicsValue>; 5]); 5] = [
        ("INPA", "SRC", [a5(), con(), none(), none(), none()]),
        ("INPA", "7", [a5(), con(), none(), none(), none()]),
        ("INPA", "", [a5(), con(), none(), none(), none()]),
        (
            "OUT",
            "SRC",
            [none(), none(), none(), con(), vec![EpicsValue::Double(0.0)]],
        ),
        (
            "PROC",
            "1",
            [a5(), none(), none(), none(), vec![EpicsValue::Double(5.0)]],
        ),
    ];
    for (field, value, want) in steps {
        db.put_record_field_from_ca_no_notify("CO", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
        for (i, f) in FIELDS.iter().enumerate() {
            // A's PROC post is `monitor()`'s alarm-cycle rule, not `special()`.
            if field == "PROC" && *f == "A" {
                drain(&mut rx[i]);
                continue;
            }
            assert_eq!(
                drain(&mut rx[i]),
                want[i],
                "caput CO.{field} {value:?}: CO.{f} posts"
            );
        }
    }
}
