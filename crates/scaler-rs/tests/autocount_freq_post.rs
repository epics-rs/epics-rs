//! C's autocount block (`scalerRecord.c:508-535`) runs after the `:470`
//! epilogue — `monitor()` (`:473`) and its `recGblResetAlarms`, VAL (`:478`),
//! the FLNK (`:480`) — so the FREQ it posts when the driver changed the clock
//! (`:530`) carries the committed alarm and follows SEVR, VAL and the FLNK
//! target's post; on a WAITING restart, which skips the epilogue, it still
//! posts. The order is read where it is decided: a filter on every subscriber
//! appends each event to one stream as it is posted.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset};
use epics_base_rs::types::DbFieldType;
use scaler_rs::MAX_SCALER_CHANNELS;
use scaler_rs::device_support::scaler_asyn::{ScalerAsynDeviceSupport, ScalerDriver};

// RTEMS-EXEC-MODEL-ALLOW(3): checked, not waived — all 3 ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p
// scaler-rs --test autocount_freq_post`). No CA server is built; the reactor
// comes from `#[tokio::test]`, which the backend does not remove.

/// `scalerSupport.dbd`'s `device(scaler,INST_IO,devScalerAsyn,"Asyn Scaler")`.
static ASYN_SCALER: DevSup = DevSup {
    name: "devScalerAsyn",
    choice: "Asyn Scaler",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(7, &[2, 4, 5, 6], false),
};

const DB: &str = r#"
record(scaler, "SC") {
    field(DTYP, "Asyn Scaler")
    field(OUT,  "@asyn(scalerPortC 0 0)")
    field(FREQ, "10000000")
    field(TP,   "1")
    field(CONT, "1")
    field(DLY1, "$(DLY1)")
    field(FLNK, "SCF")
}
record(calc, "SCF") {
    field(CALC, "A+1")
    field(INPA, "SCF")
}
"#;

/// A driver running its own clock, as the Joerger VS64 does (`:397-404`);
/// `done()` takes the shared flag.
struct OwnClock(Arc<AtomicBool>);

impl ScalerDriver for OwnClock {
    fn reset(&mut self) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    fn read(&mut self, _: &mut [u32; MAX_SCALER_CHANNELS]) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    fn write_preset(&mut self, _: usize, preset: u32) -> epics_base_rs::error::CaResult<u32> {
        Ok(preset)
    }
    fn arm(&mut self, _: bool) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    fn actual_frequency(&self) -> Option<f64> {
        Some(1.25e7)
    }
    fn done(&mut self) -> bool {
        self.0.swap(false, Ordering::SeqCst)
    }
    fn num_channels(&self) -> usize {
        1
    }
}

/// Appends `PV value severity` for each event it passes to the shared stream.
struct Stream {
    pv: &'static str,
    lines: Arc<Mutex<Vec<String>>>,
}

impl SubscriptionFilter for Stream {
    fn name(&self) -> &'static str {
        "stream"
    }

    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        let ev = &event.event;
        self.lines.lock().unwrap().push(format!(
            "{} {} {}",
            self.pv,
            ev.value().as_value().and_then(|v| v.to_f64()).unwrap(),
            ev.sevr()
        ));
        Some(event)
    }
}

type Lines = Arc<Mutex<Vec<String>>>;

/// The IOC with every `(record, field, pv)` watched into one stream, and the
/// driver's done flag.
async fn boot(
    dly1: &str,
    watched: &[(&str, &str, &'static str)],
) -> (Arc<PvDatabase>, Lines, Vec<EventReader>, Arc<AtomicBool>) {
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let (name, factory) = scaler_rs::scaler_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_device_support("scaler", &ASYN_SCALER, move || {
            Box::new(ScalerAsynDeviceSupport::new(Box::new(OwnClock(
                flag.clone(),
            ))))
        })
        .db_string(DB, &HashMap::from([("DLY1".to_string(), dly1.to_string())]))
        .unwrap()
        .build()
        .await
        .unwrap();
    let lines = Arc::new(Mutex::new(Vec::new()));
    let mut readers = Vec::new();
    for (sid, &(rec, field, pv)) in watched.iter().enumerate() {
        let cell = db.get_record(rec).unwrap();
        let mut inst = cell.write();
        readers.push(
            inst.add_subscriber(
                field,
                sid as u32,
                DbFieldType::Double,
                (EventMask::VALUE | EventMask::ALARM).bits(),
            )
            .unwrap(),
        );
        assert!(inst.attach_filter_to_last_subscriber(
            field,
            Arc::new(Stream {
                pv,
                lines: lines.clone(),
            }),
        ));
    }
    (db, lines, readers, done)
}

async fn process(db: &PvDatabase) {
    db.process_record_with_links("SC", &mut ProcStack::new())
        .await
        .unwrap();
}

/// The first idle cycle (DLY1 0): monitor() clears the init UDF INVALID, VAL
/// posts, the FLNK processes SCF, then the autocount start posts the clock.
#[tokio::test]
async fn an_autocount_freq_post_follows_the_epilogue_with_its_alarm() {
    let (db, lines, _readers, _) = boot(
        "0",
        &[
            ("SC", "SEVR", "SC.SEVR"),
            ("SC", "VAL", "SC.VAL"),
            ("SC", "FREQ", "SC.FREQ"),
            ("SCF", "VAL", "SCF"),
        ],
    )
    .await;
    process(&db).await;

    let lines = lines.lock().unwrap().clone();
    let at = |pv: &str| {
        lines
            .iter()
            .position(|l| l.starts_with(&format!("{pv} ")))
            .unwrap_or_else(|| panic!("no {pv} post in {lines:?}"))
    };
    let freq = at("SC.FREQ");
    assert_eq!(lines[freq], "SC.FREQ 12500000 0", "NO_ALARM: {lines:?}");
    for pv in ["SC.SEVR", "SC.VAL", "SCF"] {
        assert!(at(pv) < freq, "{pv} precedes FREQ: {lines:?}");
    }
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("SC.FREQ ")).count(),
        1
    );
}

/// A user count start's FREQ (`:430`) is a mark ahead of the reset, with the
/// init alarm, and nothing posts it again on the completion cycle.
#[tokio::test]
async fn a_count_start_freq_post_is_not_repeated_by_the_walk() {
    let (db, lines, _readers, done) = boot("0", &[("SC", "FREQ", "SC.FREQ")]).await;
    db.put_record_field_from_ca("SC", "CNT", epics_base_rs::types::EpicsValue::Short(1))
        .await
        .unwrap();
    done.store(true, Ordering::SeqCst);
    process(&db).await;
    let lines = lines.lock().unwrap().clone();
    assert_eq!(lines, ["SC.FREQ 12500000 3"], "one post, UDF INVALID");
}

/// DLY1 0.2: the idle cycle commits NO_ALARM and arms the restart; the
/// restart cycle is WAITING at `:470`, so it skips the epilogue, and the
/// autocount start still posts FREQ once with the committed alarm.
#[tokio::test]
async fn a_waiting_restart_posts_freq_once_with_the_committed_alarm() {
    let (db, lines, _readers, _) = boot("0.2", &[("SC", "FREQ", "SC.FREQ")]).await;
    process(&db).await;
    assert_eq!(*lines.lock().unwrap(), [] as [String; 0], "no restart yet");
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert_eq!(*lines.lock().unwrap(), ["SC.FREQ 12500000 0"]);
}
