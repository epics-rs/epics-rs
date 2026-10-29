//! A record's I/O Intr wakes reach it from whichever path put it on its
//! device's list: iocInit, a SCAN put (`scanAdd` from `dbPutSpecial`,
//! dbScan.c:236-248) or a device-link put that binds a new dset
//! (`dbPutFieldLink`'s `restoreScan`, dbAccess.c:1218-1222).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record};
use epics_base_rs::types::EpicsValue;

static PUMPED: DevSup = DevSup {
    name: "devAiPumped",
    choice: "Pumped",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], true),
};

/// Each device object's wake sender and read count, by creation order.
type Made = Arc<Mutex<Vec<(Option<mpsc::Sender<()>>, Arc<AtomicUsize>)>>>;

struct Pumped {
    id: usize,
    made: Made,
    reads: Arc<AtomicUsize>,
}

impl DeviceSupport for Pumped {
    fn io_intr_receiver(&mut self) -> Option<mpsc::Receiver<()>> {
        let (tx, rx) = mpsc::channel(4);
        self.made.lock().unwrap()[self.id].0 = Some(tx);
        Some(rx)
    }

    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "Pumped"
    }
}

async fn wake(made: &Made, id: usize) -> usize {
    let (tx, reads) = {
        let made = made.lock().unwrap();
        (
            made[id]
                .0
                .clone()
                .unwrap_or_else(|| panic!("device {id} never asked for its receiver")),
            made[id].1.clone(),
        )
    };
    let before = reads.load(Ordering::Relaxed);
    tx.send(()).await.unwrap();
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        if reads.load(Ordering::Relaxed) > before {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    reads.load(Ordering::Relaxed) - before
}

#[epics_macros_rs::epics_test]
async fn every_io_intr_join_reaches_the_record() {
    let made: Made = Arc::default();
    let factory = made.clone();
    let (db, _) = IocBuilder::new()
        .register_device_support("ai", &PUMPED, move || {
            let reads = Arc::new(AtomicUsize::new(0));
            let mut made = factory.lock().unwrap();
            made.push((None, reads.clone()));
            Box::new(Pumped {
                id: made.len() - 1,
                made: factory.clone(),
                reads,
            })
        })
        .db_string(
            r#"record(ai, "ON") { field(DTYP, "Pumped") field(INP, "@a") field(SCAN, "I/O Intr") }
record(ai, "LATER") { field(DTYP, "Pumped") field(INP, "@a") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();

    assert_eq!(wake(&made, 0).await, 1, "loaded on I/O Intr");

    db.put_pv("LATER.SCAN", EpicsValue::String("I/O Intr".into()))
        .await
        .unwrap();
    assert_eq!(wake(&made, 1).await, 1, "moved onto I/O Intr by a SCAN put");

    db.put_record_field_from_ca_no_notify("ON", "INP", EpicsValue::String("@b".into()))
        .await
        .unwrap();
    assert_eq!(made.lock().unwrap().len(), 3);
    assert_eq!(wake(&made, 2).await, 1, "relinked to a new dset");
}

static FREE: DevSup = DevSup {
    name: "devAiFree",
    choice: "Free",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], true),
};

/// A device that processes its record whatever the SCAN, and reads the
/// database's `interruptAccept` itself.
struct Free(Pumped, bool);

impl DeviceSupport for Free {
    fn bind_interrupt_gate(
        &mut self,
        _gate: &Arc<epics_base_rs::runtime::interrupt_accept::InterruptGate>,
    ) {
        self.1 = true;
    }

    fn io_intr_receiver(&mut self) -> Option<mpsc::Receiver<()>> {
        assert!(self.1, "asked for its receiver before its gate was bound");
        self.0.io_intr_receiver()
    }

    fn io_intr_scan_independent(&self) -> bool {
        true
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        self.0.read(record)
    }

    fn write(&mut self, record: &mut dyn Record) -> CaResult<()> {
        self.0.write(record)
    }

    fn dtyp(&self) -> &str {
        "Free"
    }
}

/// The dset a device-link put binds drives its Passive record as the one
/// iocInit bound did.
#[epics_macros_rs::epics_test]
async fn a_relinked_scan_independent_dset_reaches_the_record() {
    let made: Made = Arc::default();
    let factory = made.clone();
    let (db, _) = IocBuilder::new()
        .register_device_support("ai", &FREE, move || {
            let reads = Arc::new(AtomicUsize::new(0));
            let mut made = factory.lock().unwrap();
            made.push((None, reads.clone()));
            Box::new(Free(
                Pumped {
                    id: made.len() - 1,
                    made: factory.clone(),
                    reads,
                },
                false,
            ))
        })
        .db_string(
            r#"record(ai, "FREE") { field(DTYP, "Free") field(INP, "@a") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();

    assert_eq!(wake(&made, 0).await, 1, "bound at iocInit");

    db.put_record_field_from_ca_no_notify("FREE", "INP", EpicsValue::String("@b".into()))
        .await
        .unwrap();
    assert_eq!(made.lock().unwrap().len(), 2);
    assert_eq!(wake(&made, 1).await, 1, "bound by a device-link put");
}
