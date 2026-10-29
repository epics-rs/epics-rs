//! C `process` clears and posts all ten `waiting` flags at each sequence start
//! (`sseqRecord.c:339-344`), so a client sees `WTGn 0` every start, raised or not.

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

#[epics_macros_rs::epics_test]
async fn each_start_posts_wtg_zero_on_an_unused_step() {
    let db = PvDatabase::new();
    db.add_record("SQ_T", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let mut sseq = SseqRecord::new();
    sseq.put_field("DO1", EpicsValue::Double(1.0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SQ_T PP".into()))
        .unwrap();
    db.add_record("SQ", Box::new(sseq)).await.unwrap();
    let mut wtg2 = db
        .get_record("SQ")
        .unwrap()
        .write()
        .add_subscriber("WTG2", 1, DbFieldType::Short, EventMask::VALUE.bits())
        .unwrap();
    while wtg2.try_recv().is_ok() {}

    for start in 1..=2 {
        db.process_record_with_links("SQ", &mut ProcStack::new())
            .await
            .unwrap();
        // A start while BUSY is still 1 is not a start.
        let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
        while std::time::Instant::now() < deadline {
            if db.get_pv("SQ.BUSY").unwrap() == EpicsValue::Short(0) {
                break;
            }
            epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
        }
        let mut posted = Vec::new();
        while let Ok(ev) = wtg2.try_recv() {
            posted.push(ev.value().as_value().cloned());
        }
        assert_eq!(posted, [Some(EpicsValue::Short(0))], "start {start}");
    }
}
