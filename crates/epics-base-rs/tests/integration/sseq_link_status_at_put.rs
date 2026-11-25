//! sseq `special()` stores the put link's `DOLnV`/`LNKnV` before the put
//! returns (sseqRecord.c:1003-1012, :1044-1058), so a get right after the put
//! reads it. The expected values are the C oracle's for the same puts.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::{LinkRef, LinkSet, PutSplit, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::EpicsValue;

const EXT_NC: i16 = 0;
const LOCAL: i16 = 2;

const DB: &str = r#"
record(ao, "L") { field(VAL, "5") }
record(sseq, "SA") { }
record(sseq, "SB") { field(DOL1, "L") field(LNK1, "L") }
"#;

/// A CA link set whose links never connect.
struct Down;

#[epics_base_rs::async_trait]
impl LinkSet for Down {
    fn is_connected(&self, _: LinkRef<'_>) -> bool {
        false
    }
    fn get_cached_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
}

async fn build() -> Arc<PvDatabase> {
    let ioc = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_link_type("ca", |db| async move {
            db.register_link_set("ca", Arc::new(Down)).await;
            Vec::new()
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    db
}

/// Puts `link` = `to` on `rec` and reads its status under the put's hold, so
/// no task can post between the two.
fn status_right_after(db: &PvDatabase, rec: &str, link: &str, to: &str) -> i16 {
    db.put_many_locked(&[rec], 1, |held| {
        let r = held.record(0).unwrap();
        held.put_record_field_from_ca_no_notify(
            r,
            link,
            EpicsValue::String(to.into()),
            PutSplit::DbPutField,
        )
        .unwrap();
        let status = db.get_pv(format!("{rec}.{link}V")).unwrap();
        status.to_f64().unwrap() as i16
    })
}

#[epics_base_rs::epics_test]
async fn a_dol_put_naming_a_local_pv_reads_local_at_once() {
    let db = build().await;
    assert_eq!(status_right_after(&db, "SA", "DOL1", "L"), LOCAL);
}

#[epics_base_rs::epics_test]
async fn a_lnk_put_naming_a_local_pv_reads_local_at_once() {
    let db = build().await;
    assert_eq!(status_right_after(&db, "SA", "LNK1", "L"), LOCAL);
}

#[epics_base_rs::epics_test]
async fn a_dol_put_naming_a_ca_pv_reads_ext_nc_at_once() {
    let db = build().await;
    assert_eq!(status_right_after(&db, "SB", "DOL1", "R:X CA"), EXT_NC);
}

#[epics_base_rs::epics_test]
async fn a_lnk_put_naming_a_ca_pv_reads_ext_nc_at_once() {
    let db = build().await;
    assert_eq!(status_right_after(&db, "SB", "LNK1", "R:Y CA"), EXT_NC);
}
