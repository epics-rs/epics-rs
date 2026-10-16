//! A dset that makes two round trips per cycle, as devMcaAsyn does (status,
//! then data: devMcaAsyn.c:284-295), through `PortHandle::queue_request`.
//! On an `ASYN_CANBLOCK` port each trip ends its pass with PACT and the port
//! thread re-enters it, so nothing waits on the port under the record's hold
//! (the second trip used to be a `submit_blocking` there, which the debug
//! hold assert refuses). On a port without it `queueRequest` runs the callback
//! inline (asynManager.c:1555-1584) and the re-entered passes run nested:
//! three reads and three FLNKs, as C mca's three nested `process` calls.

#![cfg(feature = "epics")]

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::port_handle::{PortHandle, QueueOutcome};
use asyn_rs::request::{CellBody, PortCalls, RequestCell, RequestOp, Unrun};
use asyn_rs::runtime::{PortRuntimeHandle, RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::user::AsynUser;
use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::task::CallbackPriority;
use epics_base_rs::server::database::{DeviceDone, DeviceDoneSource, PvDatabase};
use epics_base_rs::server::device_support::{
    DeviceReadOutcome, DeviceSupport, DeviceUdf, ReadStart,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;

/// Counts its reads.
struct Counter {
    base: PortDriverBase,
    n: i32,
}

impl PortDriver for Counter {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn read_int32(&mut self, _user: &AsynUser) -> AsynResult<i32> {
        self.n += 1;
        Ok(self.n)
    }
}

fn port(name: &str, can_block: bool) -> PortRuntimeHandle {
    let flags = PortFlags {
        can_block,
        ..PortFlags::default()
    };
    let driver = Counter {
        base: PortDriverBase::new(name, 1, flags),
        n: 0,
    };
    create_port_runtime_unclaimed(driver, RuntimeConfig::default())
        .unwrap()
        .0
}

/// The callback: one driver read, then — on a port thread — the re-entry.
#[derive(Default)]
struct Trip {
    last: AtomicI32,
    reentry: Mutex<Option<(DeviceDone, CallbackPriority)>>,
}

impl CellBody for Trip {
    fn process(&self, port: &mut dyn PortCalls) {
        let got = port.call(&RequestOp::Int32Read, AsynUser::new(0)).unwrap();
        self.last.store(got.int_val.unwrap(), Ordering::SeqCst);
        if let Some((done, band)) = self.reentry.lock().unwrap().take() {
            done.reprocess(band).unwrap();
        }
    }
    fn unrun(&self, why: Unrun) {
        panic!("the trip did not run: {why:?}");
    }
    fn done(self: Arc<Self>) {}
}

type Log = Arc<Mutex<Vec<String>>>;

struct TwoTrips {
    handle: PortHandle,
    body: Arc<Trip>,
    cell: Arc<RequestCell>,
    trips: u32,
    log: Log,
}

impl TwoTrips {
    fn trip(&mut self, done: &DeviceDoneSource<'_>) -> ReadStart {
        self.trips += 1;
        if self.handle.can_block() {
            *self.body.reentry.lock().unwrap() = Some((done.device_done(), done.priority()));
        }
        match self
            .handle
            .queue_request(&self.cell, &AsynUser::new(0))
            .unwrap()
        {
            QueueOutcome::Queued => ReadStart::Pending,
            QueueOutcome::Ran => ReadStart::Reentered,
        }
    }
}

impl DeviceSupport for TwoTrips {
    fn read_begin(
        &mut self,
        _record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<ReadStart> {
        if !done.pact() {
            self.trips = 0;
        }
        Ok(if self.trips < 2 {
            self.trip(done)
        } else {
            ReadStart::Synchronous
        })
    }
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let last = self.body.last.load(Ordering::SeqCst);
        self.log.lock().unwrap().push(format!("read {last}"));
        record.set_val(EpicsValue::Double(f64::from(last)))?;
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "TwoTrips"
    }
}

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

async fn cycle(can_block: bool) -> (Arc<PvDatabase>, Vec<String>) {
    let runtime = port(if can_block { "TRIPBLK" } else { "TRIPSYNC" }, can_block);
    let handle = runtime.port_handle().clone();
    let log: Log = Arc::default();
    let l = Arc::clone(&log);
    let (db, _) = IocBuilder::new()
        .register_dynamic_device_support(move |ctx: &DeviceSupportContext| match ctx.dtyp {
            "TwoTrips" => {
                let body = Arc::new(Trip::default());
                let cell = handle.new_cell(body.clone());
                Some(Box::new(TwoTrips {
                    handle: handle.clone(),
                    body,
                    cell,
                    trips: 0,
                    log: Arc::clone(&l),
                }) as Box<dyn DeviceSupport>)
            }
            "Sink" => Some(Box::new(Sink(Arc::clone(&l))) as Box<dyn DeviceSupport>),
            _ => None,
        })
        .db_string(
            r#"
            record(ai, "AI") { field(DTYP, "TwoTrips") field(FLNK, "SINK") }
            record(ao, "SINK") { field(DTYP, "Sink") }
            "#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    db.process_record("AI").await.unwrap();
    for _ in 0..400 {
        if !db.get_record("AI").unwrap().read().is_processing() {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    assert!(!db.get_record("AI").unwrap().read().is_processing());
    // A further pass would land within this window.
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    drop(runtime);
    let log = log.lock().unwrap().clone();
    (db, log)
}

#[epics_base_rs::epics_test]
async fn a_blocking_port_takes_two_trips_without_waiting_under_the_hold() {
    let (db, log) = cycle(true).await;
    assert_eq!(log, ["read 2", "flnk"]);
    assert_eq!(db.get_pv("AI").unwrap(), EpicsValue::Double(2.0));
}

#[epics_base_rs::epics_test]
async fn a_port_that_cannot_block_runs_both_trips_nested() {
    let (db, log) = cycle(false).await;
    assert_eq!(log, ["read 2", "flnk", "read 2", "flnk", "read 2", "flnk"]);
    assert_eq!(db.get_pv("AI").unwrap(), EpicsValue::Double(2.0));
}
