//! `ReadStart::Reentered` runs the re-entered pass nested, as C mca does on a
//! port without `ASYN_CANBLOCK`: `queueRequest` runs `asynCallback` inline
//! (asynManager.c:1555-1584), which calls `prset->process` (devMcaAsyn.c:
//! 357-382), each pass with its own monitor and FLNK. The outer pass then
//! asks `read_begin` again, as C's code after that `send_msg` may send the
//! next one (mcaRecord.c:630, then the status read): a second inline callback
//! is a sibling nested pass, and a queued one leaves the record PACT.

mod declared_rows;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::task::CallbackPriority;
use epics_base_rs::server::database::{DeviceDone, DeviceDoneSource, PvDatabase};
use epics_base_rs::server::device_support::{
    DeviceReadOutcome, DeviceSupport, DeviceUdf, ReadStart,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{DbLinkType, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};

type Log = Arc<Mutex<Vec<String>>>;
type Queued = Arc<Mutex<Option<(DeviceDone, CallbackPriority)>>>;

#[derive(Clone, Copy)]
enum Answer {
    Synchronous,
    Pending,
    Reentered,
}

/// Answers its `read_begin`s from `script`, in call order across the nested
/// passes; a `Pending` keeps the completion in `queued`.
struct Scripted {
    log: Log,
    script: VecDeque<Answer>,
    queued: Queued,
    reads: f64,
}

impl DeviceSupport for Scripted {
    fn read_begin(
        &mut self,
        _record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<ReadStart> {
        self.log
            .lock()
            .unwrap()
            .push(format!("begin pact={} again={}", done.pact(), done.again()));
        Ok(match self.script.pop_front().expect("a scripted answer") {
            Answer::Synchronous => ReadStart::Synchronous,
            Answer::Reentered => ReadStart::Reentered,
            Answer::Pending => {
                *self.queued.lock().unwrap() = Some((done.device_done(), done.priority()));
                ReadStart::Pending
            }
        })
    }
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        self.reads += 1.0;
        self.log
            .lock()
            .unwrap()
            .push(format!("read {}", self.reads));
        record.set_val(EpicsValue::Double(self.reads))?;
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "Scripted"
    }
}

/// The FLNK target: logs each pass.
struct Sink(Log);

impl DeviceSupport for Sink {
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        self.0.lock().unwrap().push("flnk".into());
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "Sink"
    }
}

struct Ioc {
    db: Arc<PvDatabase>,
    log: Log,
    queued: Queued,
}

async fn ioc(script: &[Answer]) -> Ioc {
    let log: Log = Arc::default();
    let queued: Queued = Arc::default();
    let (l, q, script) = (Arc::clone(&log), Arc::clone(&queued), script.to_vec());
    let devices = [
        (
            "ai",
            declared_rows::rows(&["Scripted"], DbLinkType::Constant),
        ),
        ("ao", declared_rows::rows(&["Sink"], DbLinkType::Constant)),
    ];
    let (db, _) = IocBuilder::new()
        .register_dynamic_device_support(&devices, move |ctx: &DeviceSupportContext| {
            match ctx.dtyp {
                "Scripted" => Some(Box::new(Scripted {
                    log: Arc::clone(&l),
                    script: script.iter().copied().collect(),
                    queued: Arc::clone(&q),
                    reads: 0.0,
                }) as Box<dyn DeviceSupport>),
                "Sink" => Some(Box::new(Sink(Arc::clone(&l))) as Box<dyn DeviceSupport>),
                _ => None,
            }
        })
        .db_string(
            r#"
            record(ai, "AI") { field(DTYP, "Scripted") field(FLNK, "SINK") }
            record(ao, "SINK") { field(DTYP, "Sink") }
            "#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    Ioc { db, log, queued }
}

fn processing(db: &PvDatabase) -> bool {
    db.get_record("AI").unwrap().read().is_processing()
}

#[epics_macros_rs::epics_test]
async fn a_reentered_pass_then_synchronous_reads_in_the_outer_pass() {
    use Answer::*;
    let ioc = ioc(&[Reentered, Synchronous, Synchronous]).await;
    let mut val_rx = ioc
        .db
        .get_record("AI")
        .unwrap()
        .write()
        .add_subscriber("VAL", 1, DbFieldType::Double, EventMask::VALUE.bits())
        .unwrap();
    while val_rx.try_recv().is_ok() {}

    ioc.db.process_record("AI").await.unwrap();
    assert_eq!(
        *ioc.log.lock().unwrap(),
        [
            "begin pact=false again=false",
            // The nested pass.
            "begin pact=true again=false",
            "read 1",
            "flnk",
            // The outer pass, asked again with PACT as the nested pass left it.
            "begin pact=false again=true",
            "read 2",
            "flnk",
        ]
    );
    let mut posted = Vec::new();
    while let Ok(e) = val_rx.try_recv() {
        posted.push(e.value().as_value().cloned().unwrap());
    }
    assert_eq!(posted, [1.0, 2.0].map(EpicsValue::Double));
    assert!(!processing(&ioc.db), "the outer pass ends the cycle");
}

#[epics_macros_rs::epics_test]
async fn a_second_reentered_answer_runs_a_sibling_pass() {
    use Answer::*;
    let ioc = ioc(&[Reentered, Synchronous, Reentered, Synchronous, Synchronous]).await;
    ioc.db.process_record("AI").await.unwrap();
    assert_eq!(
        *ioc.log.lock().unwrap(),
        [
            "begin pact=false again=false",
            "begin pact=true again=false",
            "read 1",
            "flnk",
            "begin pact=false again=true",
            // The sibling, nested in the outer pass, not in the first one.
            "begin pact=true again=false",
            "read 2",
            "flnk",
            "begin pact=false again=true",
            "read 3",
            "flnk",
        ]
    );
    assert_eq!(ioc.db.get_pv("AI").unwrap(), EpicsValue::Double(3.0));
}

#[epics_macros_rs::epics_test]
async fn a_reentered_pass_then_pending_leaves_the_record_pact() {
    use Answer::*;
    let ioc = ioc(&[Reentered, Synchronous, Pending, Synchronous]).await;
    ioc.db.process_record("AI").await.unwrap();
    assert_eq!(
        *ioc.log.lock().unwrap(),
        [
            "begin pact=false again=false",
            "begin pact=true again=false",
            "read 1",
            "flnk",
            "begin pact=false again=true",
        ]
    );
    assert!(
        processing(&ioc.db),
        "the outer pass waits for its completion"
    );

    let (done, band) = ioc.queued.lock().unwrap().take().unwrap();
    std::thread::spawn(move || done.reprocess(band).unwrap())
        .join()
        .unwrap();
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        if !processing(&ioc.db) {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    assert!(!processing(&ioc.db));
    assert_eq!(
        ioc.log.lock().unwrap()[5..],
        ["begin pact=true again=false", "read 2", "flnk"]
    );
}
