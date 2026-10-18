//! A CP link's first event reads its source when `dbCaRun` releases the link,
//! as C's `CA_MONITOR_NATIVE` is serviced by `dbCaTask` only after `dbCaRun`
//! (`dbCa.c:1105-1108`, `:1209-1236`), and an I/O Intr source's read waits its
//! turn on the source's own pump, behind every interrupt queued to it first.
//! On the AD IOCs this is what puts `QueueUse` and the `Over*Link` holders'
//! first reads behind the asyn boot flush; a holder that read its UDF source
//! first latched INVALID in ACKS.

mod calink_ioc;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::interrupt_accept::EdgeDone;
use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record};
use epics_base_rs::types::EpicsValue;

static PULSE: DevSup = DevSup {
    name: "devAiPulse",
    choice: "Pulse",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], false),
};

/// An I/O Intr source whose interrupts a test sends. Its reads leave VAL
/// each value of its list in turn, the last one after that; a NaN is UDF
/// (`aiRecord.c:161`), so a first event read between two queued interrupts
/// can see it INVALID. Each read takes long enough that a first event not
/// queued behind them lands first.
struct Pulse(Option<mpsc::Receiver<()>>, &'static [f64], usize);

impl DeviceSupport for Pulse {
    fn dtyp(&self) -> &str {
        "Pulse"
    }
    fn io_intr_receiver(&mut self) -> Option<mpsc::Receiver<()>> {
        self.0.take()
    }
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let val = self.1[self.2.min(self.1.len() - 1)];
        self.2 += 1;
        std::thread::sleep(Duration::from_millis(50));
        record.put_field("VAL", EpicsValue::Double(val))?;
        Ok(DeviceReadOutcome::computed(DeviceUdf::Untouched))
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}

/// Build `db`, whose one `Pulse` record, if any, reads `vals` and takes the
/// returned sender's interrupts.
async fn build_reading(db: &str, vals: &'static [f64]) -> (Arc<PvDatabase>, mpsc::Sender<()>) {
    let (pulse, pulses) = mpsc::channel(4);
    let pulses = Mutex::new(Some(pulses));
    let (db, _) = IocBuilder::new()
        .register_device_support("ai", &PULSE, move || {
            Box::new(Pulse(pulses.lock().unwrap().take(), vals, 0))
        })
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    (db, pulse)
}

/// [`build_reading`] a source first undefined, then defined.
async fn build(db: &str) -> (Arc<PvDatabase>, mpsc::Sender<()>) {
    build_reading(db, &[f64::NAN, 1.0]).await
}

/// iocInit's CP pass, returning once its `opens` link opens have run, so a
/// link's first event could be read here.
async fn setup_cp_links(db: &PvDatabase, opens: u64) {
    db.setup_cp_links().await;
    calink_ioc::until("the links open", || {
        db.external_link_opens_completed() >= opens
    })
    .await;
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

const PULSED: &str = r#"
record(ai, "SRC") { field(DTYP, "Pulse") field(INP, "@x") field(SCAN, "I/O Intr") }
record(calc, "HOLD") { field(INPA, "SRC CP MS") field(CALC, "A") }
"#;

/// Boundary — an I/O Intr source with an interrupt pending on its pump when
/// `dbCaRun` releases the link: the first event reads it processed.
#[epics_macros_rs::epics_test]
async fn a_pending_interrupt_processes_before_the_first_event() {
    let (db, pulse) = build(PULSED).await;
    db.initialize_link_locality().await;
    setup_cp_links(&db, 1).await;
    db.scan_control().run();
    // The first poll releases the link, so its read is woken before the
    // interrupts land on the pump.
    let mut ca_run = std::pin::pin!(db.process_cp_links_on_connect());
    std::future::poll_fn(|cx| {
        let _ = ca_run.as_mut().poll(cx);
        std::task::Poll::Ready(())
    })
    .await;
    pulse.try_send(()).unwrap();
    pulse.try_send(()).unwrap();
    ca_run.await;
    db.ca_sync().await;

    assert_eq!(num(&db, "SRC.SEVR"), 0.0, "the source processed");
    assert_eq!(
        (num(&db, "HOLD.SEVR"), num(&db, "HOLD.ACKS")),
        (0.0, 0.0),
        "the holder's first event read the processed source"
    );
}

/// Boundary — the interrupt is queued by an edge job that is still running
/// when `scanRun` returns: `dbCaRun`'s step runs behind the job.
#[epics_macros_rs::epics_test]
async fn an_edge_job_queues_before_the_first_event() {
    let (db, pulse) = build(PULSED).await;
    db.initialize_link_locality().await;
    setup_cp_links(&db, 1).await;
    db.scan_control().interrupt_gate().on_open(move || {
        let pulse = pulse.clone();
        EdgeDone::Job {
            name: "late-flush",
            job: Box::new(move || {
                std::thread::sleep(Duration::from_millis(200));
                pulse.try_send(()).unwrap();
                pulse.try_send(()).unwrap();
            }),
        }
    });
    db.scan_control().run();
    db.process_cp_links_on_connect().await;
    db.ca_sync().await;

    assert_eq!(num(&db, "SRC.SEVR"), 0.0, "the source processed");
    assert_eq!(
        (num(&db, "HOLD.SEVR"), num(&db, "HOLD.ACKS")),
        (0.0, 0.0),
        "the holder's first event read the processed source"
    );
}

/// Boundary — a source with no pump changed between the links' setup and
/// `dbCaRun`, by a process that posted nothing (MDEL): the first event is
/// read at `dbCaRun`, so it carries the change.
#[epics_macros_rs::epics_test]
async fn a_source_with_no_pump_is_read_at_ca_run() {
    let (db, _) = build(
        r#"
record(ao, "SRC") { field(MDEL, "100") }
record(calc, "HOLD") { field(INPA, "SRC CP") field(CALC, "A") }
"#,
    )
    .await;
    db.initialize_link_locality().await;
    db.put_record_field_from_ca("SRC", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
    setup_cp_links(&db, 1).await;
    db.put_record_field_from_ca("SRC", "VAL", EpicsValue::Double(2.0))
        .await
        .unwrap();
    db.scan_control().run();
    db.process_cp_links_on_connect().await;
    db.ca_sync().await;

    assert_eq!(num(&db, "SRC"), 2.0);
    assert_eq!(
        num(&db, "HOLD.A"),
        2.0,
        "the first event read SRC at dbCaRun"
    );
}

/// Boundary — a link opened after `iocRun` to an I/O Intr source with
/// interrupts pending behind one in flight: the same turn on the source's
/// pump, so neither its first event nor a later one reads the undefined
/// pass between them.
#[epics_macros_rs::epics_test]
async fn a_link_opened_after_ioc_run_takes_the_pumps_turn() {
    let (db, pulse) = build_reading(
        r#"
record(ai, "SRC") { field(DTYP, "Pulse") field(INP, "@x") field(SCAN, "I/O Intr") }
record(calc, "HOLD") { field(CALC, "A") }
"#,
        &[1.0, f64::NAN, 1.0],
    )
    .await;
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;

    // The open waits out the read in flight; two more are queued by then.
    pulse.try_send(()).unwrap();
    db.put_record_field_from_ca("HOLD", "INPA", EpicsValue::String("SRC CP MS".into()))
        .await
        .unwrap();
    pulse.try_send(()).unwrap();
    pulse.try_send(()).unwrap();
    calink_ioc::until("the source and the holder process", || {
        num(&db, "SRC.SEVR") == 0.0 && num(&db, "HOLD.UDF") == 0.0
    })
    .await;
    db.ca_sync().await;

    assert_eq!(
        (num(&db, "HOLD.SEVR"), num(&db, "HOLD.ACKS")),
        (0.0, 0.0),
        "the new link's first event read the processed source"
    );
}

/// Boundary — interrupts keep arriving while the link waits its turn: the
/// turn comes after those queued when it was asked for, not once the queue
/// runs dry, so `dbCaRun` returns.
#[epics_macros_rs::epics_test]
async fn a_stream_of_interrupts_does_not_hold_the_turn() {
    let (db, pulse) = build(PULSED).await;
    db.initialize_link_locality().await;
    setup_cp_links(&db, 1).await;
    db.scan_control().run();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let streaming = stop.clone();
    let stream = std::thread::spawn(move || {
        while !streaming.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = pulse.try_send(());
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    epics_base_rs::runtime::task::timeout(
        fact_budget::FACT_BUDGET,
        db.process_cp_links_on_connect(),
    )
    .await
    .expect("dbCaRun returns while the source streams");
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    stream.join().unwrap();
}
