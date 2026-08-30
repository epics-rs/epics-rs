//! What mask a `.BUSY` event carries, not merely that one arrives.
//!
//! C posts BUSY from the DEVICE SUPPORT, never from the record.
//! `devAsynXXXTimeSeries.h:154-157` (asyn e2a281e2):
//!
//! ```c
//! if (pwf->busy != busy) {
//!   pwf->busy = busy;
//!   db_post_events(pwf, &pwf->busy, DBE_VALUE | DBE_LOG);
//! ```
//!
//! The third argument is a LITERAL `DBE_VALUE | DBE_LOG`. It is not
//! `monitor_mask`, so it never carries `recGblResetAlarms`'s alarm-transition
//! bit — and `waveformRecord.c` posts BUSY nowhere at all (its only four
//! `db_post_events` are NORD at `waveformRecord.c:148` and `:213`, HASH at
//! `:319`, VAL at `:324`), so this is the whole of C's BUSY posting.
//!
//! The port reaches BUSY through the generic change-detecting subscriber walk,
//! whose default aux mask is `alarm_bits | DBE_VALUE | DBE_LOG`
//! (`record_instance.rs::collect_subscriber_posts`). On any cycle that also
//! moves the alarm — the very first process of a record does, born-UDF to
//! NO_ALARM — that default adds a `DBE_ALARM` bit C does not send. A
//! DBE_ALARM-only subscriber to `.BUSY` therefore woke up on arming, where the
//! C IOC delivers it nothing.
//!
//! Asserting delivery would not have caught it: the event arrives either way.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use asyn_rs::asyn_record::register_port;
use asyn_rs::param::ParamType;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::create_port_runtime;
use asyn_rs::trace::TraceManager;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// Every bit, so the assertion reads the mask the IOC chose rather than the
/// one the subscription filtered down to.
const ALL: u16 = 0x07;

const REC: &str = "TS:MASK";

struct TsPort {
    base: PortDriverBase,
}

impl TsPort {
    fn new(name: &str) -> Self {
        let mut base = PortDriverBase::new(name, 1, PortFlags::default());
        base.create_param("VAL", ParamType::Int32).unwrap();
        Self { base }
    }
}

impl PortDriver for TsPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

/// A waveform bound to `asynInt32TimeSeries`, with a live port behind it, and
/// a `.BUSY` subscriber listening on every mask bit.
async fn armed_time_series(port: &str) -> (Arc<PvDatabase>, EventReader) {
    time_series_with_nelm(port, 8).await
}

/// `armed_time_series` with an explicit NELM — the NELM=1 case matters because
/// C seeds `prec->nord = (prec->nelm == 1)` in `init_record`
/// (`waveformRecord.c:100`), so such a record is born with NORD=1 and the
/// first arm (which commits the empty buffer) moves it 1 -> 0 on the very
/// cycle that carries the born-UDF alarm transition.
async fn time_series_with_nelm(port: &str, nelm: u32) -> (Arc<PvDatabase>, EventReader) {
    let (runtime, _join) = create_port_runtime(TsPort::new(port), RuntimeConfig::default())
        .expect("port runtime starts");
    register_port(
        port,
        runtime.port_handle().clone(),
        Arc::new(TraceManager::new()),
    )
    .expect("port name is free");
    // Dropping the runtime handle must not kill a registered port — that is
    // `port_runtime_lifetime.rs`'s contract, and this test leans on it.
    drop(runtime);

    let db_text = format!(
        r#"record(waveform, "{REC}") {{
    field(DTYP, "asynInt32TimeSeries")
    field(INP,  "@asyn({port},0)VAL")
    field(FTVL, "LONG")
    field(NELM, "{nelm}")
    field(SCAN, "Passive")
}}"#
    );
    let (database, _) = register_asyn(IocBuilder::new())
        .db_string(&db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let reader = {
        let inst = database.get_record(REC).unwrap();
        let mut g = inst.write();
        assert_eq!(
            g.record.get_field("BUSY"),
            Some(EpicsValue::Short(0)),
            "the record must start un-armed or the transition below is not one"
        );
        g.add_subscriber("BUSY", 1, DbFieldType::Short, ALL)
            .expect("BUSY is a readable field and must accept a subscription")
    };
    (database, reader)
}

fn register_asyn(b: IocBuilder) -> IocBuilder {
    asyn_rs::adapter::register_asyn_device_support_for_builder(b)
}

/// `caput TS:MASK.RARM <v>` — the `dbPutField` route, whose pp gate is what
/// processes a Passive record after a write to a `pp(TRUE)` field. `put_pv` is
/// the bare `dbPut` and would leave the record unprocessed.
async fn caput_rarm(
    db: &PvDatabase,
    v: i16,
) -> epics_base_rs::error::CaResult<epics_base_rs::server::record::ProcessCompletion> {
    db.put_record_field_from_ca(REC, "RARM", EpicsValue::Short(v))
        .await
}

/// `caput TS:MASK.RARM 1` — RARM is `pp(TRUE)`, so the put processes the
/// record, the dset's RARM switch arms it, and BUSY moves 0 -> 1. This is also
/// the record's FIRST process, so `recGblResetAlarms` returns `DBE_ALARM` for
/// the born-UDF -> NO_ALARM transition: the cycle where the port's default aux
/// mask and C's literal disagree.
#[epics_base_rs::epics_test]
async fn arming_posts_busy_with_dbe_value_and_dbe_log_only() {
    let (db, mut rx) = armed_time_series("mask_ts_arm").await;

    caput_rarm(&db, 1)
        .await
        .expect("RARM is client-settable (waveformRecord.dbd.pod:410-414 pp(TRUE))");

    let event = rx
        .try_recv()
        .expect("BUSY changed 0 -> 1, so C's `if (pwf->busy != busy)` fires");
    assert_eq!(
        event.snapshot.value,
        EpicsValue::Short(1),
        "the armed value is what C stores before posting"
    );
    assert_eq!(
        event.mask,
        EventMask::VALUE | EventMask::LOG,
        "devAsynXXXTimeSeries.h:156 passes a literal DBE_VALUE|DBE_LOG; the \
         alarm bit this cycle belongs to VAL's post, not to BUSY's"
    );
}

/// The disarm half, on a cycle with no alarm movement — same literal mask.
/// Included because a fix that merely dropped the alarm bit on the *first*
/// process would still be a special case; the rule is per-field and uniform.
#[epics_base_rs::epics_test]
async fn disarming_posts_busy_with_the_same_literal_mask() {
    let (db, mut rx) = armed_time_series("mask_ts_disarm").await;

    caput_rarm(&db, 1).await.unwrap();
    let arm = rx.try_recv().expect("arm event");
    assert_eq!(arm.snapshot.value, EpicsValue::Short(1));

    // RARM = 2 is C's "stop": `busy = 0`.
    caput_rarm(&db, 2).await.unwrap();
    let disarm = rx.try_recv().expect("BUSY changed 1 -> 0");
    assert_eq!(disarm.snapshot.value, EpicsValue::Short(0));
    assert_eq!(
        disarm.mask,
        EventMask::VALUE | EventMask::LOG,
        "the same literal, on a cycle whose alarm did not move"
    );
}

/// BUSY is change-gated in C (`if (pwf->busy != busy)`): re-arming an already
/// armed series posts nothing. Pinned so the mask fix above cannot be mistaken
/// for permission to post BUSY unconditionally.
#[epics_base_rs::epics_test]
async fn re_arming_an_armed_series_posts_no_busy_event() {
    let (db, mut rx) = armed_time_series("mask_ts_rearm").await;

    caput_rarm(&db, 1).await.unwrap();
    rx.try_recv().expect("the 0 -> 1 transition");

    // RARM = 3 is C's "resume": `busy = 1` — already 1, so `pwf->busy != busy`
    // is false and C's db_post_events is not reached.
    caput_rarm(&db, 3).await.unwrap();
    epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
    assert!(
        rx.try_recv().is_err(),
        "BUSY did not change, so C posts nothing"
    );
}

/// NORD is the other half of the same C fact and the same one-line fix:
/// `waveformRecord.c:148` and `devAsynXXXTimeSeries.h:152` both pass the
/// literal.
///
/// NELM=1 is what makes this cycle exist. `init_record` seeds
/// `prec->nord = (prec->nelm == 1)`, so the record is born with NORD=1; the
/// first arm commits the (empty) accumulator, `pPvt->nord` is 0, and
/// `devAsynXXXTimeSeries.h:150` fires — on the same cycle `recGblResetAlarms`
/// reports the born-UDF -> NO_ALARM transition. With any larger NELM the two
/// can never coincide: the callback appends only while BUSY, so no sample can
/// move NORD until after the arm that clears the alarm.
#[epics_base_rs::epics_test]
async fn the_first_arm_of_a_one_element_series_posts_nord_with_the_literal_mask() {
    let (db, _busy) = time_series_with_nelm("mask_ts_nord", 1).await;
    let mut nord = {
        let inst = db.get_record(REC).unwrap();
        let mut g = inst.write();
        assert_eq!(
            g.record.get_field("NORD"),
            Some(EpicsValue::ULong(1)),
            "waveformRecord.c:100 seeds NORD = (NELM == 1)"
        );
        g.add_subscriber("NORD", 2, DbFieldType::Long, ALL)
            .expect("NORD is readable")
    };

    caput_rarm(&db, 1).await.unwrap();

    let ev = nord
        .try_recv()
        .expect("NORD moved 1 -> 0 on the arming commit");
    assert_eq!(ev.snapshot.value, EpicsValue::ULong(0));
    assert_eq!(
        ev.mask,
        EventMask::VALUE | EventMask::LOG,
        "waveformRecord.c:148 / devAsynXXXTimeSeries.h:152 pass the literal"
    );
}

/// RARM is written by the device support and posted by nobody.
///
/// `devAsynXXXTimeSeries.h:179` is a bare `pwf->rarm = 0;` — no
/// `db_post_events` follows it, and `waveformRecord.c`'s four post sites
/// (NORD `waveformRecord.c:148`/`:213`, HASH `:319`, VAL `:324`) do not name
/// RARM either. So
/// the whole of C's RARM posting is `dbPut`'s own field tail
/// (`dbAccess.c:1406-1413`), which fires on the client's write and carries
/// `DBE_VALUE | DBE_LOG`: a `camonitor TS:MASK.RARM` shows 1 when the caput
/// lands and never returns to 0.
///
/// The port reached RARM through the generic change-detecting walk, which saw
/// the dset's reset and invented a second event.
#[epics_base_rs::epics_test]
async fn the_dsets_rarm_reset_posts_nothing() {
    let (db, _busy) = armed_time_series("mask_ts_rarm").await;
    let mut rarm = {
        let inst = db.get_record(REC).unwrap();
        let mut g = inst.write();
        g.add_subscriber("RARM", 3, DbFieldType::Short, ALL)
            .expect("RARM is readable")
    };

    caput_rarm(&db, 1).await.unwrap();

    let put = rarm
        .try_recv()
        .expect("dbPut's field tail posts the client's own write");
    assert_eq!(put.snapshot.value, EpicsValue::Short(1));
    assert_eq!(put.mask, EventMask::VALUE | EventMask::LOG);

    assert_eq!(
        db.get_pv(&format!("{REC}.RARM")).unwrap(),
        EpicsValue::Short(0),
        "the dset did reset it (devAsynXXXTimeSeries.h:179) — the field moved"
    );
    assert!(
        rarm.try_recv().is_err(),
        "...and C posts nothing for that reset, so a camonitor stays at 1"
    );
}
