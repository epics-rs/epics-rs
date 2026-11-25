//! A database a library serves without any build — no IocBuilder, no
//! iocInit, so no `callbackInit` — still processes an I/O Intr record whose
//! scan-independent device a device-link put bound: each device wake reaches
//! a CA monitor of the record.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record, register_device_object_row};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::protocol::CA_PROTO_EVENT_ADD;
use raw_ca::*;

static WOKEN: DevSup = DevSup {
    name: "devAiServedWoken",
    choice: "ServedWoken",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], true),
};

/// Reads its wake count into VAL, whatever the record's SCAN.
struct Woken {
    wakes: Arc<Mutex<Option<mpsc::Sender<()>>>>,
    reads: AtomicUsize,
}

impl DeviceSupport for Woken {
    fn io_intr_receiver(&mut self) -> Option<mpsc::Receiver<()>> {
        let (tx, rx) = mpsc::channel(4);
        *self.wakes.lock().unwrap() = Some(tx);
        Some(rx)
    }

    fn io_intr_scan_independent(&self) -> bool {
        true
    }

    fn io_intr_processes_directly(&self) -> bool {
        true
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let n = self.reads.fetch_add(1, Ordering::Relaxed) + 1;
        record.put_field("VAL", EpicsValue::Double(n as f64))?;
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "ServedWoken"
    }
}

fn put(db: &PvDatabase, field: &str, value: &str) {
    block_on_sync(db.put_record_field_from_ca_no_notify(
        "R",
        field,
        EpicsValue::String(value.into()),
    ))
    .expect("no async runtime on this thread")
    .unwrap_or_else(|e| panic!("R.{field}: {e}"));
}

#[test]
fn a_device_wake_reaches_a_monitor_of_the_unbuilt_database() {
    register_device_object_row("ai", &WOKEN);
    let wakes = Arc::new(Mutex::new(None));
    let db = Arc::new(PvDatabase::new());
    let made = wakes.clone();
    db.install_device_support_resolver(Arc::new(move |ctx| {
        (ctx.dtyp == "ServedWoken").then(|| {
            Box::new(Woken {
                wakes: made.clone(),
                reads: AtomicUsize::new(0),
            }) as Box<dyn DeviceSupport>
        })
    }));
    block_on_sync(db.add_record("R", Box::new(AiRecord::new(0.0))))
        .expect("no async runtime on this thread")
        .expect("add_record");
    put(&db, "SCAN", "I/O Intr");
    put(&db, "DTYP", "ServedWoken");
    put(&db, "INP", "@a");
    let wake = wakes
        .lock()
        .unwrap()
        .clone()
        .expect("the INP put bound the device");

    let (server, addr, accept) = start_server(db.clone());
    let mut c = connect_and_handshake(addr);
    let sid = create_channel(&mut c, 1, "R");
    c.send(&event_add_frame(sid, 1));
    assert_eq!(
        payload_double(&c.expect(CA_PROTO_EVENT_ADD, "R's initial")),
        0.0
    );

    wake.try_send(()).unwrap();
    assert_eq!(
        payload_double(&c.expect(CA_PROTO_EVENT_ADD, "R processed by the wake")),
        1.0
    );

    server.shutdown();
    let _ = accept.join();
}
