//! A CA link's status follows its connection, by the rule its record's C
//! uses.
//!
//! Base `calcoutRecord.c` reads `dbIsLinkConnected` for every volatile link —
//! a `CA` link to a record of this IOC included — in `init_record` and in the
//! `checkLinks` `process()` runs while a CA link exists, and while one stays
//! unconnected `checkLinksCallback` re-reads every 0.5 s; init schedules no
//! re-check, so a calcout that never processes keeps `Ext PV NC`. The synApps
//! calcs classify by `dbNameToAddr` — a local name is `Local PV` whatever its
//! modifier — and re-check the rest 1 s after init.
//!
//! Measured on oracleIoc (EPICS 7.0.10 + calc R3-7-5): a processed calcout
//! reads `INPA "K:SRC CA"` as `Ext PV OK`, an unprocessed one `Ext PV NC`,
//! and scalcout reads it `Local PV`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use epics_base_rs::server::database::{LinkRef, LinkSet, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const EXT_NC: i16 = 0;
const EXT_OK: i16 = 1;
const LOC: i16 = 2;

const DB: &str = r#"
record(ao, "K:SRC") { field(VAL, "1") }
record(calcout, "K:CN") { field(INPA, "R:X CA") field(OUT, "R:X CA") field(CALC, "A") }
record(calcout, "K:CL") { field(INPA, "K:SRC CA") field(INPB, "K:SRC") field(CALC, "A") }
record(scalcout, "K:S") { field(INPA, "K:SRC CA") field(INPB, "R:X CA") field(CALC, "A") }
"#;

/// The CA link set, connected as its flag says.
struct Lset(Arc<AtomicBool>);

#[epics_base_rs::async_trait]
impl LinkSet for Lset {
    fn is_connected(&self, _: LinkRef<'_>) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn get_cached_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
}

/// `iocInit` whole: `iocBuild`, then `iocRun`, whose `dbCaRun`
/// (iocInit.c:260) is what lets any CA link read connected.
async fn build(up: &Arc<AtomicBool>) -> Arc<PvDatabase> {
    let up = up.clone();
    let ioc = IocBuilder::new()
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .register_link_type("ca", |db| async move {
            db.register_link_set("ca", Arc::new(Lset(up))).await;
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

fn read(db: &PvDatabase, pv: &str) -> i16 {
    db.get_pv(pv)
        .ok()
        .and_then(|v| v.to_f64())
        .map(|f| f as i16)
        .unwrap_or_else(|| panic!("{pv} not readable as a number"))
}

async fn settled(db: &PvDatabase, pv: &str, want: i16) {
    for _ in 0..400 {
        if db.get_pv(pv).ok().and_then(|v| v.to_f64()) == Some(f64::from(want)) {
            return;
        }
        sleep(5).await;
    }
    panic!("{pv} never reached {want}: {:?}", db.get_pv(pv));
}

async fn sleep(ms: u64) {
    epics_base_rs::runtime::task::sleep(Duration::from_millis(ms)).await;
}

/// Init schedules no re-check: past the synApps 1 s one, an unprocessed
/// calcout still reads `Ext PV NC`; its next process moves every connected
/// link, a CA link to a local name included, and posts the move.
#[epics_macros_rs::epics_test]
async fn a_calcout_reads_a_connected_ca_link_ok_once_it_processes() {
    let up = Arc::new(AtomicBool::new(false));
    let db = build(&up).await;
    settled(&db, "K:CL.INBV", LOC).await;
    up.store(true, Ordering::SeqCst);
    sleep(1500).await;
    for pv in ["K:CN.INAV", "K:CN.OUTV", "K:CL.INAV"] {
        assert_eq!(read(&db, pv), EXT_NC, "{pv}");
    }

    let rec = db.get_record("K:CN").unwrap();
    let mut rx = rec
        .write()
        .add_subscriber("INAV", 1, DbFieldType::Enum, EventMask::VALUE.bits())
        .expect("subscriber");
    while rx.try_recv().is_ok() {}
    db.process_record("K:CN").await.unwrap();
    db.process_record("K:CL").await.unwrap();
    for pv in ["K:CN.INAV", "K:CN.OUTV", "K:CL.INAV"] {
        assert_eq!(read(&db, pv), EXT_OK, "{pv}");
    }
    assert_eq!(read(&db, "K:CL.INBV"), LOC);
    let posted: Vec<EpicsValue> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|ev| ev.value().as_value().cloned())
        .collect();
    assert_eq!(posted, [EpicsValue::Enum(EXT_OK as u16)]);
}

/// Once `checkLinks` has run, a link that drops reads `Ext PV NC` at the next
/// process, and the 0.5 s `checkLinksCallback` that process arms moves it
/// back once it reconnects, with no further process. (A calcout whose links
/// all connected by `init_record` keeps `NO_CA_LINKS` and never re-reads
/// them, as in C.)
#[epics_macros_rs::epics_test]
async fn a_dropped_calcout_link_is_rechecked_until_it_reconnects() {
    let up = Arc::new(AtomicBool::new(false));
    let db = build(&up).await;
    settled(&db, "K:CL.INBV", LOC).await;
    up.store(true, Ordering::SeqCst);
    db.process_record("K:CN").await.unwrap();
    assert_eq!(read(&db, "K:CN.INAV"), EXT_OK);
    up.store(false, Ordering::SeqCst);
    db.process_record("K:CN").await.unwrap();
    assert_eq!(read(&db, "K:CN.INAV"), EXT_NC);
    sleep(700).await;
    assert_eq!(read(&db, "K:CN.INAV"), EXT_NC);
    up.store(true, Ordering::SeqCst);
    settled(&db, "K:CN.INAV", EXT_OK).await;
    assert_eq!(read(&db, "K:CN.OUTV"), EXT_OK);
}

/// scalcout reads a CA link to a local name `Local PV`, and its 1 s init
/// re-check moves a remote link that connected meanwhile.
#[epics_macros_rs::epics_test]
async fn a_scalcout_ca_link_is_local_by_name_and_rechecked_after_init() {
    let up = Arc::new(AtomicBool::new(false));
    let db = build(&up).await;
    settled(&db, "K:S.INAV", LOC).await;
    assert_eq!(read(&db, "K:S.INBV"), EXT_NC);
    up.store(true, Ordering::SeqCst);
    settled(&db, "K:S.INBV", EXT_OK).await;
    assert_eq!(read(&db, "K:S.INAV"), LOC);
}
