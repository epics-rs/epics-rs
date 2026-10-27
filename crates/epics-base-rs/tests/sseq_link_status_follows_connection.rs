//! sseq's `DOLnV`/`LNKnV` follow a CA link's connection through C
//! `checkLinksCallback` (`sseqRecord.c:825-967`): `init_record` arms it 1 s
//! out while a link is `Ext PV NC`, each pass moves a CA link between
//! `Ext PV NC` and `Ext PV OK`, takes its `DTn`/`LTn` from the channel and
//! re-posts every `Local PV`, and it re-arms every 0.5 s until no CA link is
//! down. `process()` never runs it.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::{
    BackgroundTaskHandle, CallbackPriority, sleep_background, spawn_background,
};
use epics_base_rs::server::database::{LinkDbfType, LinkMetadata, LinkRef, LinkSet, PvDatabase};
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const EXT_NC: i16 = 0;
const EXT_OK: i16 = 1;
const LOC: i16 = 2;
/// C dbStatic `DBF_DOUBLE`, what `dbGetLinkDBFtype` of the test's channels is.
const DBF_DOUBLE: i16 = 10;

const DB: &str = r#"
record(ao, "Q:SRC") { field(VAL, "1") }
record(sseq, "Q:SQ") {
    field(DOL1, "R:X CA") field(LNK1, "R:Y CA")
    field(DOL2, "Q:SRC") field(LNK2, "Q:SRC.VAL") field(WAIT2, "Wait")
}
record(sseq, "Q:SL") {
    field(DOL1, "Q:SRC") field(LNK1, "Q:SRC.VAL") field(WAIT1, "Wait")
}
record(sseq, "Q:SP") { field(WAIT1, "Wait") }
record(sseq, "Q:SN") { field(DOL1, "R:Z") field(LNK1, "R:W.VAL") field(WAIT1, "Wait") }
"#;

/// The CA link set, connected as its flag says, every channel a double.
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
    fn link_metadata(&self, _: LinkRef<'_>) -> Option<LinkMetadata> {
        self.0.load(Ordering::SeqCst).then(|| LinkMetadata {
            dbf_type: Some(LinkDbfType::Double),
            ..Default::default()
        })
    }
}

async fn build(up: &Arc<AtomicBool>) -> Arc<PvDatabase> {
    let up = up.clone();
    let ioc = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
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
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while Instant::now() < deadline {
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

/// What `pv` reads 600 ms on the delayed-callback timer after a put armed
/// its pass, read on cbLow. Queued from cbMedium, behind the `special` task
/// that arms the pass there, so the timer, firing in deadline order, runs a
/// 0.5 s pass first however late the host runs both.
fn read_after_600_ms(db: &Arc<PvDatabase>, pv: &'static str) -> BackgroundTaskHandle<i16> {
    let db = db.clone();
    spawn_background(CallbackPriority::Medium, async move {
        spawn_background(CallbackPriority::Low, async move {
            sleep_background(Duration::from_millis(600)).await;
            read(&db, pv)
        })
        .await
        .unwrap()
    })
}

fn drain(rx: &mut EventReader) -> usize {
    let mut n = 0;
    while rx.try_recv().is_ok() {
        n += 1;
    }
    n
}

/// When a cbLow task takes each of the next `n` posts to `rx`, then each one
/// queued 600 ms on the delayed-callback timer after the last; a 0.5 s
/// re-arm's pass is among those. The last post wakes this on cbLow's one
/// worker once its pass has re-armed, and the timer fires in deadline order,
/// so the next pass posts first however late the host runs both.
fn posts_then_600_ms(
    mut rx: EventReader,
    n: usize,
) -> BackgroundTaskHandle<(EventReader, Vec<Instant>)> {
    spawn_background(CallbackPriority::Low, async move {
        let mut seen = Vec::new();
        for _ in 0..n {
            rx.recv().await.expect("subscriber open");
            seen.push(Instant::now());
        }
        sleep_background(Duration::from_millis(600)).await;
        let at = Instant::now();
        seen.extend((0..drain(&mut rx)).map(|_| at));
        (rx, seen)
    })
}

/// A timer never fires early and a re-arm counts from its own pass, so with
/// the chain armed by init after `armed`, pass k posts no sooner than
/// `armed` + 1 s + (k - 1) × 0.5 s. `seen[i]` is pass i + 1 or a later one.
fn assert_no_pass_early(armed: Instant, seen: &[Instant]) {
    for (i, &at) in seen.iter().enumerate() {
        let due = armed + Duration::from_secs(1) + Duration::from_millis(500) * i as u32;
        assert!(at >= due, "pass {} posted {:?} early", i + 1, due - at);
    }
}

/// Init leaves both CA links `Ext PV NC` with no type; the pass init arms
/// moves them to `Ext PV OK` and takes the channel's type, with no process.
#[epics_macros_rs::epics_test]
async fn an_sseq_ca_link_reads_ok_once_connected_without_a_process() {
    let up = Arc::new(AtomicBool::new(false));
    let db = build(&up).await;
    settled(&db, "Q:SQ.DOL2V", LOC).await;
    for pv in ["Q:SQ.DOL1V", "Q:SQ.LNK1V"] {
        assert_eq!(read(&db, pv), EXT_NC, "{pv}");
    }
    assert_eq!(read(&db, "Q:SQ.LT1"), -1);
    up.store(true, Ordering::SeqCst);
    settled(&db, "Q:SQ.LNK1V", EXT_OK).await;
    assert_eq!(read(&db, "Q:SQ.DOL1V"), EXT_OK);
    assert_eq!(read(&db, "Q:SQ.DT1"), DBF_DOUBLE);
    assert_eq!(read(&db, "Q:SQ.LT1"), DBF_DOUBLE);
    assert_eq!(read(&db, "Q:SQ.LNK2V"), LOC);
}

/// While a CA link stays down every 0.5 s pass re-posts the `Local PV`
/// statuses; the pass that finds it connected arms no further one.
#[epics_macros_rs::epics_test]
async fn a_down_sseq_link_is_rechecked_until_it_connects() {
    let up = Arc::new(AtomicBool::new(false));
    let armed = Instant::now();
    let db = build(&up).await;
    settled(&db, "Q:SQ.DOL2V", LOC).await;
    let rec = db.get_record("Q:SQ").unwrap();
    let rx = rec
        .write()
        .add_subscriber("LNK2V", 1, DbFieldType::Enum, EventMask::VALUE.bits())
        .expect("subscriber");
    // A put's pass, asked while the recheck is pending, arms no second one.
    db.put_pv_and_post("Q:SQ.WAIT2", EpicsValue::String("Wait".into()))
        .await
        .unwrap();
    let (mut rx, seen) = posts_then_600_ms(rx, 5).await.unwrap();
    assert!(seen.len() > 5, "no pass 600 ms after the fifth");
    assert_no_pass_early(armed, &seen);
    assert_eq!(read(&db, "Q:SQ.LNK1V"), EXT_NC);

    up.store(true, Ordering::SeqCst);
    settled(&db, "Q:SQ.LNK1V", EXT_OK).await;
    sleep(100).await;
    while rx.try_recv().is_ok() {}
    sleep(1200).await;
    assert!(
        rx.try_recv().is_err(),
        "a pass ran after every link connected"
    );
}

/// A `WAITn` put runs a pass: `WERRn` follows the wait asked of a DB link,
/// and a connected CA link keeps `Ext PV OK`.
#[epics_macros_rs::epics_test]
async fn a_wait_put_keeps_a_connected_ca_link_ok() {
    let up = Arc::new(AtomicBool::new(true));
    let db = build(&up).await;
    settled(&db, "Q:SQ.LNK1V", EXT_OK).await;
    assert_eq!(read(&db, "Q:SQ.WERR2"), 1);
    db.put_pv_and_post("Q:SQ.WAIT1", EpicsValue::String("Wait".into()))
        .await
        .unwrap();
    db.put_pv_and_post("Q:SQ.WAIT2", EpicsValue::String("NoWait".into()))
        .await
        .unwrap();
    settled(&db, "Q:SQ.WERR2", 0).await;
    sleep(100).await;
    assert_eq!(read(&db, "Q:SQ.LNK1V"), EXT_OK);
    assert_eq!(read(&db, "Q:SQ.LT1"), DBF_DOUBLE);
    assert_eq!(read(&db, "Q:SQ.WERR1"), 0);
}

/// `init_record` computes no `WERRn`: the 1 s `checkLinksCallback` does
/// (`sseqRecord.c:347`), and with it every constant link's `DTn` goes
/// `DBF_unknown`.
#[epics_macros_rs::epics_test]
async fn init_leaves_werr_and_types_to_the_1s_pass() {
    let up = Arc::new(AtomicBool::new(true));
    // Before init arms the pass, so the pass is at least 1 s after it.
    let start = Instant::now();
    let db = build(&up).await;
    settled(&db, "Q:SQ.DOL2V", LOC).await;
    assert_eq!(read(&db, "Q:SQ.WERR2"), 0);
    assert_ne!(read(&db, "Q:SQ.DT3"), -1);
    settled(&db, "Q:SQ.WERR2", 1).await;
    assert!(
        start.elapsed() >= Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(read(&db, "Q:SQ.DT3"), -1);
}

/// With no CA link C never schedules a pass, so a `Wait` on a DB link keeps
/// `WERRn` 0.
#[epics_macros_rs::epics_test]
async fn an_all_local_sseq_runs_no_pass() {
    let up = Arc::new(AtomicBool::new(true));
    let db = build(&up).await;
    settled(&db, "Q:SL.LNK1V", LOC).await;
    sleep(1500).await;
    assert_eq!(read(&db, "Q:SL.WERR1"), 0);
}

/// A `WAITn` put arms the pass 0.5 s out (`sseqRecord.c:1092`).
#[epics_macros_rs::epics_test]
async fn a_wait_put_moves_werr_after_half_a_second() {
    let up = Arc::new(AtomicBool::new(true));
    let db = build(&up).await;
    settled(&db, "Q:SQ.WERR2", 1).await;
    sleep(600).await;
    let put = Instant::now();
    db.put_pv_and_post("Q:SQ.WAIT2", EpicsValue::String("NoWait".into()))
        .await
        .unwrap();
    let werr = read_after_600_ms(&db, "Q:SQ.WERR2");
    settled(&db, "Q:SQ.WERR2", 0).await;
    assert!(
        put.elapsed() >= Duration::from_millis(500),
        "{:?}",
        put.elapsed()
    );
    assert_eq!(werr.await.unwrap(), 0, "WERR2 600 ms on");
}

/// An `LNKn` put posts its status at once and arms the pass that sets
/// `WERRn` 0.5 s out (`sseqRecord.c:1073`).
#[epics_macros_rs::epics_test]
async fn a_link_put_sets_werr_after_half_a_second() {
    let up = Arc::new(AtomicBool::new(true));
    let db = build(&up).await;
    sleep(1500).await;
    assert_eq!(read(&db, "Q:SP.WERR1"), 0);
    let put = Instant::now();
    db.put_record_field_from_ca("Q:SP", "LNK1", EpicsValue::String("Q:SRC.VAL".into()))
        .await
        .unwrap();
    let werr = read_after_600_ms(&db, "Q:SP.WERR1");
    settled(&db, "Q:SP.LNK1V", LOC).await;
    settled(&db, "Q:SP.WERR1", 1).await;
    assert!(
        put.elapsed() >= Duration::from_millis(500),
        "{:?}",
        put.elapsed()
    );
    assert_eq!(werr.await.unwrap(), 1, "WERR1 600 ms on");
}

/// A DB-syntax link naming no record here is the `CA_LINK` C's `dbInitLink`
/// falls through to (`dbLink.c:118-130`), so `checkLinks` rechecks it.
#[epics_macros_rs::epics_test]
async fn a_plain_link_to_a_name_not_here_is_rechecked_as_ca() {
    let up = Arc::new(AtomicBool::new(false));
    let db = build(&up).await;
    sleep(200).await;
    assert_eq!(read(&db, "Q:SN.DOL1V"), EXT_NC);
    assert_eq!(read(&db, "Q:SN.LNK1V"), EXT_NC);
    up.store(true, Ordering::SeqCst);
    settled(&db, "Q:SN.LNK1V", EXT_OK).await;
    assert_eq!(read(&db, "Q:SN.DOL1V"), EXT_OK);
    assert_eq!(read(&db, "Q:SN.DT1"), DBF_DOUBLE);
    assert_eq!(read(&db, "Q:SN.WERR1"), 0);
}

/// A put of such a name is the same `CA_LINK` (`dbAddLink`): `special`
/// posts it `Ext PV NC`, and the pass 0.5 s out finds it connected and
/// rescinds the `WERRn` a DB link would carry.
#[epics_macros_rs::epics_test]
async fn a_put_of_a_name_not_here_is_rechecked_as_ca() {
    let up = Arc::new(AtomicBool::new(true));
    let db = build(&up).await;
    db.put_record_field_from_ca("Q:SP", "LNK1", EpicsValue::String("R:V.VAL".into()))
        .await
        .unwrap();
    settled(&db, "Q:SP.LNK1V", EXT_NC).await;
    settled(&db, "Q:SP.LNK1V", EXT_OK).await;
    assert_eq!(read(&db, "Q:SP.LT1"), DBF_DOUBLE);
    sleep(600).await;
    assert_eq!(read(&db, "Q:SP.WERR1"), 0);
    db.put_record_field_from_ca("Q:SP", "LNK1", EpicsValue::String("Q:SRC.VAL".into()))
        .await
        .unwrap();
    settled(&db, "Q:SP.LNK1V", LOC).await;
    settled(&db, "Q:SP.WERR1", 1).await;
}

/// After the init chain has stopped, a `DOLn` put naming a down CA link arms
/// its own pass (`sseqRecord.c:1018`), which re-arms until the link connects.
#[epics_macros_rs::epics_test]
async fn a_put_of_a_down_ca_link_after_init_is_rechecked() {
    let up = Arc::new(AtomicBool::new(true));
    let db = build(&up).await;
    settled(&db, "Q:SQ.LNK1V", EXT_OK).await;
    sleep(1200).await;
    up.store(false, Ordering::SeqCst);
    db.put_field_from_client(
        "Q:SQ",
        "DOL3",
        EpicsValue::String("R:Z CA".into()),
        epics_base_rs::server::database::ProcessMode::Inhibit,
        false,
    )
    .await
    .unwrap();
    settled(&db, "Q:SQ.DOL3V", EXT_NC).await;
    sleep(300).await;
    up.store(true, Ordering::SeqCst);
    settled(&db, "Q:SQ.DOL3V", EXT_OK).await;
}
