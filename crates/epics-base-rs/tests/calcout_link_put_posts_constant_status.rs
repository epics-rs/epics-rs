//! A runtime put to a calcout link posts what C's `special()` is WRITTEN to
//! post (calcoutRecord.c:366-405): that link's status field, classified, and —
//! for a link left constant — the value field behind it (A..U, or OVAL for OUT). No other status field,
//! and nothing on the next process.
//!
//! This is not softIoc's stream. C runs that `special()` before `dbAddLink`
//! sets the `lset` (dbAccess.c:1181-1208), so softIoc R7.0.10 reads every
//! re-point as Constant and loads nothing; R19-1
//! (`calcout_constant_input_reseed.rs`) keeps the intended behaviour instead.
//! The tree that pins softIoc's own stream is
//! `/tmp/claude-1000/-home-stevek-work/561a1cf1-a6b9-439c-be95-16435c98199a/scratchpad/handoff/calcout-c-stream.patch`.
//!
//! ```text
//! record(ai, "SRC") { field(VAL, "3") }
//! record(calcout, "CO") { field(CALC, "A") field(INPA, "5") }
//!
//! caput CO.INPA SRC   ->  CO.INAV Local PV
//! caput CO.INPA 7     ->  CO.A 7     CO.INAV Constant
//! caput CO.INPA ""    ->  CO.A 7     CO.INAV Constant
//! caput CO.OUT SRC    ->  CO.OUTV Local PV
//! caput CO.PROC 1     ->  CO.A 7     CO.OVAL 7        (no INAV/INBV/OUTV)
//! caput CO.OUT ""     ->  CO.OVAL 7  CO.OUTV Constant
//! caput CO.OUT 5      ->  CO.OVAL 7  CO.OUTV Constant
//! ```

use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const LOCAL: u16 = 2;
const CONSTANT: u16 = 3;
const FIELDS: [&str; 5] = ["A", "INAV", "INBV", "OUTV", "OVAL"];

fn drain(rx: &mut EventReader) -> Vec<EpicsValue> {
    let mut got = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        got.push(ev.snapshot.value.clone());
    }
    got
}

#[epics_macros_rs::epics_test]
async fn calcout_link_puts_post_the_intended_stream() {
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

    let a7 = || vec![EpicsValue::Double(7.0)];
    let st = |s: u16| vec![EpicsValue::Enum(s)];
    let none = Vec::<EpicsValue>::new;
    let steps: [(&str, &str, [Vec<EpicsValue>; 5]); 7] = [
        ("INPA", "SRC", [none(), st(LOCAL), none(), none(), none()]),
        ("INPA", "7", [a7(), st(CONSTANT), none(), none(), none()]),
        ("INPA", "", [a7(), st(CONSTANT), none(), none(), none()]),
        ("OUT", "SRC", [none(), none(), none(), st(LOCAL), none()]),
        ("PROC", "1", [a7(), none(), none(), none(), a7()]),
        ("OUT", "", [none(), none(), none(), st(CONSTANT), a7()]),
        ("OUT", "5", [none(), none(), none(), st(CONSTANT), a7()]),
    ];
    for (field, value, want) in steps {
        db.put_record_field_from_ca_no_notify("CO", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
        // The status classification is posted by a spawned task.
        epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
        for (i, f) in FIELDS.iter().enumerate() {
            assert_eq!(
                drain(&mut rx[i]),
                want[i],
                "caput CO.{field} {value:?}: CO.{f} posts"
            );
        }
    }
}
