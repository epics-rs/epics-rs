//! The mca's device half on both port kinds, as devMcaAsyn drives it. On a
//! port without `ASYN_CANBLOCK` each callback runs inside its send and the
//! pass it re-enters runs nested; the outer pass then goes on from that
//! `send_msg` (`mcaRecord.c:678`, `:777`), asked again by the framework. On a
//! CANBLOCK port each read queues and its callback's pass lands it.
//!
//! A MODE put's PHA is `mcaData` (`mcaRecord.c:630`): devMcaAsyn sets RDNG
//! (`devMcaAsyn.c:287`) and its callback re-enters the record, which reads
//! the spectrum (`:524`, `:746-753`).

// RTEMS-EXEC-MODEL-ALLOW(7): checked, not waived — runs on the exec backend
// (`EPICS_RS_BUILD_EXEC_BACKEND=thread`); no CA server is built.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::task::CallbackPriority;
use epics_base_rs::server::database::{DeviceDone, DeviceDoneSource, PvDatabase};
use epics_base_rs::server::device_support::{
    DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, ReadStart,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record};
use epics_base_rs::types::{EpicsValue, WallTime};
use mca_rs::record::{McaCommand, McaDset, McaRead, McaRecord, McaStatus, Nack, Sent};

static PORT: [DevSup; 1] = [DevSup {
    name: "devMcaPort",
    choice: "Port",
    link_type: DbLinkType::Constant,
    dset: Dset::new(6, &[2, 3, 4, 5], true),
}];

static SINK: [DevSup; 1] = [DevSup {
    name: "devAoSink",
    choice: "Sink",
    link_type: DbLinkType::Constant,
    dset: Dset::new(6, &[2, 3, 4], false),
}];

type Log = Arc<Mutex<Vec<&'static str>>>;
type Owed = Arc<Mutex<Vec<(DeviceDone, CallbackPriority)>>>;

/// devMcaAsyn's reads against a port that either runs each callback inside
/// the send or queues it for [`Owed`].
struct Port {
    can_block: bool,
    reentry: Option<(DeviceDone, CallbackPriority)>,
    owed: Owed,
    log: Log,
}

impl McaDset for Port {
    fn send_msg(&mut self, _command: McaCommand) -> Result<(), Nack> {
        Ok(())
    }
    fn send_read(&mut self, read: McaRead) -> Result<Sent, Nack> {
        self.log.lock().unwrap().push(match read {
            McaRead::Status => "send_status",
            McaRead::Data => "send_data",
        });
        if !self.can_block {
            return Ok(Sent::Ran);
        }
        self.owed
            .lock()
            .unwrap()
            .push(self.reentry.clone().expect("each read re-enters"));
        Ok(Sent::Queued)
    }
    fn read_status(&mut self, _mca: &McaRecord) -> Result<McaStatus, Nack> {
        Ok(McaStatus::default())
    }
    fn read_array(&mut self, mca: &mut McaRecord) -> Result<(), Nack> {
        self.log.lock().unwrap().push("read_array");
        mca.land_spectrum_read(EpicsValue::LongArray(vec![1, 2, 3, 4].into()))
            .map_err(|_| Nack)
    }
}

struct DevPort {
    port: Port,
}

impl DeviceSupport for DevPort {
    fn dtyp(&self) -> &str {
        "Port"
    }
    fn init(&mut self, _record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        Ok(DeviceInitOutcome::ok())
    }
    fn read_begin(
        &mut self,
        record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<ReadStart> {
        self.port.reentry = Some((done.device_done(), done.priority()));
        let mca = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<McaRecord>())
            .unwrap();
        Ok(mca.device_read_begin(&mut self.port, done))
    }
    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::ok())
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}

/// Logs each FLNK the mca makes.
struct Sink(Log);

impl DeviceSupport for Sink {
    fn dtyp(&self) -> &str {
        "Sink"
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        self.0.lock().unwrap().push("flnk");
        Ok(())
    }
}

struct Ioc {
    db: Arc<PvDatabase>,
    log: Log,
    owed: Owed,
}

async fn ioc(can_block: bool) -> Ioc {
    ioc_with(can_block, "").await
}

/// [`ioc`] with `fields` added to the mca, beside an `ai` `SRC` to point a
/// TSEL at.
async fn ioc_with(can_block: bool, fields: &str) -> Ioc {
    let log: Log = Arc::default();
    let owed: Owed = Arc::default();
    let (l, o) = (Arc::clone(&log), Arc::clone(&owed));
    let (name, factory) = mca_rs::mca_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(
            &[("mca", &PORT), ("ao", &SINK)],
            move |ctx: &DeviceSupportContext| match ctx.dtyp {
                "Port" => Some(Box::new(DevPort {
                    port: Port {
                        can_block,
                        reentry: None,
                        owed: Arc::clone(&o),
                        log: Arc::clone(&l),
                    },
                }) as Box<dyn DeviceSupport>),
                "Sink" => Some(Box::new(Sink(Arc::clone(&l))) as Box<dyn DeviceSupport>),
                _ => None,
            },
        )
        .db_string(
            &format!(
                r#"
            record(mca, "M") {{ field(DTYP, "Port") field(NMAX, "4") field(NUSE, "4")
                               field(FLNK, "SINK") {fields} }}
            record(ao, "SINK") {{ field(DTYP, "Sink") }}
            record(ai, "SRC") {{ }}
            "#
            ),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    log.lock().unwrap().clear();
    Ioc { db, log, owed }
}

fn val(db: &PvDatabase) -> EpicsValue {
    db.get_pv("M").unwrap()
}

async fn put(db: &PvDatabase, field: &str, value: &str) {
    db.put_record_field_from_ca("M", field, EpicsValue::String(value.into()))
        .await
        .unwrap();
}

/// Runs the oldest owed callback and waits for the log to reach `len`.
async fn complete_one(ioc: &Ioc, len: usize) {
    let (done, band) = ioc.owed.lock().unwrap().remove(0);
    std::thread::spawn(move || done.reprocess(band).unwrap())
        .join()
        .unwrap();
    for _ in 0..400 {
        if ioc.log.lock().unwrap().len() >= len {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("log stopped at {:?}", ioc.log.lock().unwrap());
}

#[tokio::test]
async fn a_status_read_run_inline_resumes_the_outer_pass_once() {
    let ioc = ioc(false).await;
    put(&ioc.db, "PROC", "1").await;
    // The status callback's pass, nested, then the outer pass (`:678`).
    assert_eq!(*ioc.log.lock().unwrap(), ["send_status", "flnk", "flnk"]);
}

#[tokio::test]
async fn a_read_run_inline_nests_three_passes_and_reads_twice() {
    let ioc = ioc(false).await;
    put(&ioc.db, "READ", "1").await;
    assert_eq!(
        *ioc.log.lock().unwrap(),
        [
            "send_status",
            // The status pass starts the data read; its pass nests in it.
            "send_data",
            "read_array",
            "flnk",
            // The status pass after its send (`:777`) reads again.
            "read_array",
            "flnk",
            // The outer pass (`:678`).
            "flnk",
        ]
    );
    assert_eq!(val(&ioc.db), EpicsValue::LongArray(vec![1, 2, 3, 4].into()));
}

#[tokio::test]
async fn a_queued_read_lands_in_its_callback_pass() {
    let ioc = ioc(true).await;
    put(&ioc.db, "READ", "1").await;
    assert_eq!(*ioc.log.lock().unwrap(), ["send_status"]);
    complete_one(&ioc, 2).await;
    assert_eq!(ioc.log.lock().unwrap()[1..], ["send_data"]);
    complete_one(&ioc, 4).await;
    assert_eq!(ioc.log.lock().unwrap()[2..], ["read_array", "flnk"]);
    assert_eq!(val(&ioc.db), EpicsValue::LongArray(vec![1, 2, 3, 4].into()));
}

#[tokio::test]
async fn a_pha_mode_put_run_inline_reads_and_links_three_times() {
    let ioc = ioc(false).await;
    put(&ioc.db, "MODE", "PHA").await;
    assert_eq!(
        *ioc.log.lock().unwrap(),
        [
            "send_data",
            // The data callback's pass, nested in the put's.
            "read_array",
            "flnk",
            "send_status",
            // The status callback's pass, a sibling.
            "flnk",
            // The put's own pass.
            "flnk",
        ]
    );
    assert_eq!(val(&ioc.db), EpicsValue::LongArray(vec![1, 2, 3, 4].into()));
}

#[tokio::test]
async fn a_pha_mode_put_queued_reads_in_the_first_callback_pass() {
    let ioc = ioc(true).await;
    put(&ioc.db, "MODE", "PHA").await;
    assert_eq!(*ioc.log.lock().unwrap(), ["send_data", "send_status"]);
    assert_eq!(ioc.owed.lock().unwrap().len(), 2, "one per read");

    // The data callback's pass enters at `read_status:` (`:523`).
    complete_one(&ioc, 4).await;
    assert_eq!(ioc.log.lock().unwrap()[2..], ["read_array", "flnk"]);
    assert_eq!(val(&ioc.db), EpicsValue::LongArray(vec![1, 2, 3, 4].into()));

    // The status callback's pass sends a fresh status read.
    complete_one(&ioc, 5).await;
    assert_eq!(ioc.log.lock().unwrap()[4..], ["send_status"]);
    complete_one(&ioc, 6).await;
    assert_eq!(ioc.log.lock().unwrap()[5..], ["flnk"]);
}

/// Processes `SRC` for a fresh TIME, a few ms after the last.
async fn stamp_src(db: &PvDatabase) -> WallTime {
    std::thread::sleep(Duration::from_millis(3));
    db.process_record("SRC").await.unwrap();
    db.get_record("SRC").unwrap().read().common.time
}

/// RTIM as C computes it from `pmca->time` (`mcaRecord.c:763`).
fn epics_seconds(t: WallTime) -> f64 {
    let secs = t.unix_secs() - 631_152_000;
    secs as f64 + f64::from(t.subsec_nanos()) * 1.0e-9
}

/// STIM's `:%S.` (`:812`), which no zone offset moves; the 25-byte buffer
/// leaves no room for `%03f`, so epicsTime writes `**` after it.
fn stim_seconds(t: WallTime) -> String {
    format!(":{:02}.**", t.unix_secs() % 60)
}

fn stamps(db: &PvDatabase) -> (f64, String, WallTime) {
    let EpicsValue::String(stim) = db.get_pv("M.STIM").unwrap() else {
        panic!("STIM is a string");
    };
    (
        db.get_pv("M.RTIM").unwrap().to_f64().unwrap(),
        stim.to_string(),
        db.get_record("M").unwrap().read().common.time,
    )
}

/// C sets RTIM and STIM from `pmca->time` right after `recGblGetTimeStamp`
/// (`:762-764`, `:811-814`), so a `.TIME` TSEL drives both.
#[tokio::test]
async fn rtim_and_stim_read_a_time_tsel_stamp() {
    let ioc = ioc_with(false, r#"field(TSEL, "SRC.TIME")"#).await;
    let src = stamp_src(&ioc.db).await;
    std::thread::sleep(Duration::from_millis(3));
    put(&ioc.db, "READ", "1").await;
    let (rtim, stim, time) = stamps(&ioc.db);
    assert_eq!(time, src);
    assert_eq!(rtim, epics_seconds(src));
    assert!(stim.ends_with(&stim_seconds(src)), "{stim}");
}

/// On a CANBLOCK port the pass that queues the data read stamps before it
/// waits (`:762`, `:778`): RTIM keeps that stamp, and the data callback's
/// pass stamps STIM from the TIME it finds then (`:811`).
#[tokio::test]
async fn a_queued_read_keeps_its_read_begin_rtim() {
    let ioc = ioc_with(true, r#"field(TSEL, "SRC.TIME")"#).await;
    let begun = stamp_src(&ioc.db).await;
    put(&ioc.db, "READ", "1").await;
    complete_one(&ioc, 2).await;
    assert_eq!(ioc.log.lock().unwrap()[1..], ["send_data"]);
    assert_eq!(stamps(&ioc.db).2, begun, "the queuing pass stamped");

    // STIM shows whole seconds only.
    std::thread::sleep(Duration::from_millis(1001));
    let stopped = stamp_src(&ioc.db).await;
    complete_one(&ioc, 4).await;
    let (rtim, stim, time) = stamps(&ioc.db);
    assert_eq!(rtim, epics_seconds(begun));
    assert_eq!(time, stopped);
    assert!(stim.ends_with(&stim_seconds(stopped)), "{stim}");
}
