//! A `time=true` external link adopts the upstream timestamp only when the
//! `.db` declares `field(TSE,"-2")` — the port must not infer it.
//!
//! pvxs writes exactly two fields in that branch, `precord->time` and
//! `precord->utag` (`pvxs/ioc/pvalink_lset.cpp:269-272`), and never
//! `precord->utag`'s neighbour `precord->tse` — there is no assignment to it
//! anywhere in pvxs or in epics-base. Whether the adopted pair then SURVIVES
//! is `recGblGetTimeStampSimm`'s business, and with the default `TSE=0` it
//! restamps `time` from the clock. That is why pvxs's own test database
//! declares `field(TSE,"-2")` on the records that use the option
//! (`pvxs/test/testpvalink.db:140,230`).
//!
//! The port used to write `-2` itself, which both put a value into a
//! client-readable field that no database declared and made the adoption
//! happen on records that never asked for it.
//!
//! `utag` is the asymmetry that pins the shape: `recGblGetTimeStampSimm`
//! touches `time` and not `utag`, so the tag survives in BOTH rows while the
//! timestamp survives only in the declared one.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use epics_base_rs::server::database::{LinkSet, PvDatabase};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;

/// A stamp no clock in this process produces.
const REMOTE_SECS: i64 = 4_100_000;
const REMOTE_NSEC: i32 = 246_802_468;
const REMOTE_UTAG: u64 = 0x5A5A_0001;

/// Stub `pva` lset with the `time=true` option on: it answers `time_stamp`,
/// which is the whole gate — the real lset returns `None` without the option.
struct TimedLset;

#[epics_base_rs::async_trait]
impl LinkSet for TimedLset {
    fn is_connected(&self, _: &str) -> bool {
        true
    }
    fn get_cached_value(&self, _: &str) -> Option<EpicsValue> {
        Some(EpicsValue::Double(3.0))
    }
    async fn get_value(&self, name: &str) -> Option<EpicsValue> {
        self.get_cached_value(name)
    }
    fn time_stamp(&self, _: &str) -> Option<(i64, i32, u64)> {
        Some((REMOTE_SECS, REMOTE_NSEC, REMOTE_UTAG))
    }
}

fn remote_stamp() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::new(REMOTE_SECS as u64, REMOTE_NSEC as u32)
}

#[epics_macros_rs::epics_test]
async fn the_declared_tse_decides_whether_the_remote_time_survives() {
    let db = PvDatabase::new();
    db.register_link_set("pva", Arc::new(TimedLset)).await;

    // Same link, same lset; the ONLY difference is the declared TSE.
    for (name, tse) in [("TSEP_UNDEC", 0i16), ("TSEP_DEV", -2i16)] {
        db.add_record(name, Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let rec = db.get_record(name).expect("record added");
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("pva://REMOTE:PV".into()))
            .unwrap();
        inst.put_common_field("TSE", EpicsValue::Short(tse))
            .unwrap();
        inst.common.udf = 0;
    }

    for name in ["TSEP_UNDEC", "TSEP_DEV"] {
        let mut visited = HashSet::new();
        db.process_record_with_links(name, &mut visited, 0)
            .await
            .unwrap();
    }

    let read = |name: &str| {
        let rec = db.get_record(name).expect("record exists");
        let inst = rec.read();
        (
            inst.common.time,
            inst.common.utag,
            inst.client_field_value("TSE").expect("TSE resolves"),
        )
    };

    let (time, utag, tse) = read("TSEP_DEV");
    assert_eq!(
        time,
        remote_stamp(),
        "field(TSE,\"-2\") is what makes the upstream stamp survive"
    );
    assert_eq!(utag, REMOTE_UTAG, "the tag comes with it");
    assert_eq!(
        tse,
        EpicsValue::Short(-2),
        "and the declared TSE is unchanged"
    );

    let (time, utag, tse) = read("TSEP_UNDEC");
    assert_ne!(
        time,
        remote_stamp(),
        "TSE=0 means `epicsTimeGetCurrent`, so the adopted time is restamped"
    );
    assert_eq!(
        utag, REMOTE_UTAG,
        "`recGblGetTimeStampSimm` never touches utag, so the tag survives either way"
    );
    assert_eq!(
        tse,
        EpicsValue::Short(0),
        "the port must not write TSE on a record whose .db left it at 0"
    );
}
