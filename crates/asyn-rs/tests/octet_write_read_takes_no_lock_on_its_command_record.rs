//! `asynOctetWriteRead` reads its command record with C `dbGet(DBR_STRING)`
//! (devAsynOctet.c:878-899), which takes no lock. On a port that cannot block
//! the read runs under the asyn record's own hold, so a holder of the command
//! record that waits for the asyn record's lock set must not deadlock the two.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::error::AsynResult;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{PortRuntimeHandle, RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::types::EpicsValue;

/// Answers each read with the last bytes written, and keeps every write.
struct Echo {
    base: PortDriverBase,
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl PortDriver for Echo {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        self.writes.lock().unwrap().push(data.to_vec());
        Ok(data.len())
    }
    fn read_octet(
        &mut self,
        _user: &AsynUser,
        buf: &mut [u8],
    ) -> asyn_rs::interpose::OctetReadResult {
        let writes = self.writes.lock().unwrap();
        let last = writes.last().map_or(&[][..], Vec::as_slice);
        let n = last.len().min(buf.len());
        buf[..n].copy_from_slice(&last[..n]);
        asyn_rs::interpose::OctetReadResult::ok(n, asyn_rs::interpose::EomReason::empty())
    }
}

fn sync_port(name: &str, db: &PvDatabase, writes: &Arc<Mutex<Vec<Vec<u8>>>>) -> PortRuntimeHandle {
    let base = PortDriverBase::new(
        name,
        1,
        PortFlags::default(),
        InterfaceMask::OCTET,
        InterfaceMask::NONE,
    );
    let driver = Echo {
        base,
        writes: Arc::clone(writes),
    };
    let (runtime, _actor) =
        create_port_runtime_unclaimed(driver, RuntimeConfig::default()).unwrap();
    asyn_rs::registry::register_port(
        name,
        runtime.port_handle().clone(),
        db.scan_control().interrupt_gate(),
    )
    .unwrap();
    runtime
}

/// `name` processed on a thread of its own.
fn process(db: &Arc<PvDatabase>, name: &'static str) -> std::thread::JoinHandle<()> {
    let db = db.clone();
    std::thread::spawn(move || {
        epics_base_rs::runtime::task::test_block_on(async move {
            db.process_record(name).await.unwrap();
        })
    })
}

async fn join_within(threads: Vec<std::thread::JoinHandle<()>>, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !threads.iter().all(|t| t.is_finished()) {
        assert!(Instant::now() < deadline, "the threads deadlocked");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    for t in threads {
        t.join().unwrap();
    }
}

#[epics_base_rs::epics_test]
async fn a_held_command_record_neither_blocks_nor_deadlocks_the_read() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let db = Arc::new(PvDatabase::new());
    let _port = sync_port("WRNL", &db, &writes);
    let records = r#"
        record(stringout, "CMD") { field(VAL, "ID?") }
        record(stringin, "SI") { field(DTYP, "asynOctetWriteRead") field(INP, "@asyn(WRNL,0,1)CMD") }
    "#;
    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
        .with_database(db)
        .db_string(records, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();
    let h = db.async_handle();

    // CMD held for a while elsewhere: the read is answered at its release.
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let holder = {
        let h = h.clone();
        std::thread::spawn(move || {
            h.with_record("CMD", |_| {
                held_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(300));
            });
        })
    };
    held_rx.recv().unwrap();
    join_within(vec![holder, process(&db, "SI")], 20).await;
    assert_eq!(db.get_pv("SI").unwrap(), EpicsValue::String("ID?".into()));
    assert_eq!(writes.lock().unwrap().len(), 1);

    // CMD's holder then waits for SI's set, which SI's read holds: C's
    // unlocked dbGet cannot be had, so the read fails rather than wait.
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let holder = {
        let h = h.clone();
        std::thread::spawn(move || {
            h.with_record("CMD", |_| {
                held_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                h.with_record("SI", |_| {});
            });
        })
    };
    held_rx.recv().unwrap();
    let reader = process(&db, "SI");
    // SI is inside its read of CMD by now.
    epics_base_rs::runtime::task::sleep(Duration::from_millis(500)).await;
    go_tx.send(()).unwrap();
    join_within(vec![holder, reader], 20).await;
    assert_eq!(
        (db.get_pv("SI.STAT").unwrap(), db.get_pv("SI.SEVR").unwrap()),
        (
            EpicsValue::Short(alarm_status::READ_ALARM as i16),
            EpicsValue::Short(3)
        )
    );
    assert_eq!(writes.lock().unwrap().len(), 1);
}

/// The command record is the one whose processing reached the read, and so
/// is held by the reading thread itself: it is read, not refused.
#[epics_base_rs::epics_test]
async fn a_command_record_this_thread_holds_is_read() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let db = Arc::new(PvDatabase::new());
    let _port = sync_port("WRCH", &db, &writes);
    let records = r#"
        record(stringin, "RD") { field(VAL, "B") field(INP, "SI PP") }
        record(stringin, "SI") { field(DTYP, "asynOctetWriteRead") field(INP, "@asyn(WRCH,0,1)RD") }
    "#;
    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
        .with_database(db)
        .db_string(records, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();
    db.process_record("RD").await.unwrap();
    assert_eq!(db.get_pv("SI").unwrap(), EpicsValue::String("B".into()));
    assert_eq!(db.get_pv("SI.STAT").unwrap(), EpicsValue::Short(0));
    assert_eq!(*writes.lock().unwrap(), vec![b"B".to_vec()]);
}

/// A read waiting on the command record's holder still answers another
/// holder's read of its own record: SI2's command is SI, read while SI waits
/// for CMD. C's unlocked `dbGet` of a record another thread is processing
/// never fails.
#[epics_base_rs::epics_test]
async fn a_waiting_read_answers_a_read_of_its_own_record() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let writes2 = Arc::new(Mutex::new(Vec::new()));
    let db = Arc::new(PvDatabase::new());
    let _port = sync_port("WRPA", &db, &writes);
    let _port2 = sync_port("WRPB", &db, &writes2);
    let records = r#"
        record(stringout, "CMD") { field(VAL, "ID?") }
        record(stringin, "SI") { field(DTYP, "asynOctetWriteRead") field(INP, "@asyn(WRPA,0,1)CMD") field(VAL, "old") }
        record(stringin, "SI2") { field(DTYP, "asynOctetWriteRead") field(INP, "@asyn(WRPB,0,1)SI") }
    "#;
    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
        .with_database(db)
        .db_string(records, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();
    let h = db.async_handle();

    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        h.with_record("CMD", |_| {
            held_tx.send(()).unwrap();
            go_rx.recv().unwrap();
        });
    });
    held_rx.recv().unwrap();
    let si = process(&db, "SI");
    // SI is waiting in its read of CMD by now.
    epics_base_rs::runtime::task::sleep(Duration::from_millis(300)).await;
    join_within(vec![process(&db, "SI2")], 20).await;
    go_tx.send(()).unwrap();
    join_within(vec![holder, si], 20).await;
    assert_eq!(
        (db.get_pv("SI2.STAT").unwrap(), db.get_pv("SI2").unwrap()),
        (EpicsValue::Short(0), EpicsValue::String("old".into()))
    );
    assert_eq!(*writes2.lock().unwrap(), vec![b"old".to_vec()]);
    assert_eq!(db.get_pv("SI").unwrap(), EpicsValue::String("ID?".into()));
}
