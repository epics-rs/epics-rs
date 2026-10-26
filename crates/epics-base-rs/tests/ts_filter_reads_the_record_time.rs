//! A `ts` value mode on a read reports the time the source holds, not the
//! time of the read.
//!
//! C `db_create_read_log` (`dbEvent.c:760-770`) starts every read log from
//! `db_create_field_log`, which copies `prec->time` (`dbEvent.c:704-713`), and
//! `ts`'s value modes run on the post chain (`ts.c:322-336`) over that log's
//! `time`. Only `generate` (`{"ts":{}}`) stamps the clock. The port built a
//! value-only read log with `now()` as its time, so `{"ts":{"num":"nsec"}}`
//! on a link or a `dbChannelGet` read the clock.

use epics_base_rs::server::database::filters::db_channel_create;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::{EpicsValue, WallTime};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

type Db = Arc<epics_base_rs::server::database::PvDatabase>;

/// A source stamp no read's clock produces: its nanoseconds are what a `ts`
/// `nsec` read must report.
const SRC_STAMP: Duration = Duration::new(1_000_000_000, 333_333_333);
const SRC_NSEC: u32 = 333_333_333;

const DB: &str = r#"
record(ai, "TSR_SRC")       { field(VAL, "1") }
record(calc, "TSR_HOST")    { field(CALC, "B") }
record(longin, "TSR_REC")   { field(INP, "TSR_SRC.VAL{\"ts\":{\"num\":\"nsec\"}}") }
record(longin, "TSR_SHADOW") { field(INP, "TSR_HOST.B{\"ts\":{\"num\":\"nsec\"}}") }
record(longin, "TSR_PV")    { field(INP, "TSR_LONE.{\"ts\":{\"num\":\"nsec\"}}") }
"#;

async fn ioc() -> Db {
    let db: Db = IocBuilder::new()
        .pv("TSR_HOST.B", EpicsValue::Double(1.0))
        .pv("TSR_LONE", EpicsValue::Double(1.0))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    // `TSR_SRC` is never processed, so the planted stamp stays put; the
    // simple PVs hold a posted snapshot with the same stamp.
    db.get_record("TSR_SRC").unwrap().write().common.time = WallTime::UNIX_EPOCH + SRC_STAMP;
    for pv in ["TSR_HOST.B", "TSR_LONE"] {
        db.find_pv(pv).unwrap().set_snapshot(Snapshot::new(
            EpicsValue::Double(1.0),
            0,
            0,
            WallTime::UNIX_EPOCH + SRC_STAMP,
        ));
    }
    db
}

async fn link_read(db: &Db, reader: &str) -> f64 {
    db.process_record(reader).await.unwrap();
    db.get_pv(reader).unwrap().to_f64().unwrap()
}

#[epics_macros_rs::epics_test]
async fn a_link_to_a_record_reads_the_record_time() {
    let db = ioc().await;
    assert_eq!(link_read(&db, "TSR_REC").await, SRC_NSEC as f64);
}

#[epics_macros_rs::epics_test]
async fn a_link_to_a_shadowing_simple_pv_reads_its_posted_time() {
    let db = ioc().await;
    assert_eq!(link_read(&db, "TSR_SHADOW").await, SRC_NSEC as f64);
}

#[epics_macros_rs::epics_test]
async fn a_link_to_a_simple_pv_reads_its_posted_time() {
    let db = ioc().await;
    assert_eq!(link_read(&db, "TSR_PV").await, SRC_NSEC as f64);
}

#[epics_macros_rs::epics_test]
async fn a_db_channel_get_reads_the_record_time() {
    let db = ioc().await;
    let channel = db_channel_create(&db, b"TSR_SRC.VAL{\"ts\":{\"num\":\"nsec\"}}").unwrap();
    assert_eq!(channel.get(&db), Some(EpicsValue::ULong(SRC_NSEC)));
}
