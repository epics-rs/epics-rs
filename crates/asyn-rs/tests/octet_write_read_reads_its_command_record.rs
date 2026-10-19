//! `DTYP "asynOctetWriteRead"`: the link's userParam names a record whose
//! value is the command. C `initDbAddr` resolves it at init (a missing one
//! kills the record at LINK/INVALID), and each process reads it with
//! `dbGet(DBR_STRING)`, escape-translates it, writes it and reads the reply
//! (devAsynOctet.c:645-657, :878-907). The DTYP used to resolve to no read
//! operation, so the record processed and did nothing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

fn port(
    name: &str,
    can_block: bool,
    db: &PvDatabase,
    writes: &Arc<Mutex<Vec<Vec<u8>>>>,
) -> PortRuntimeHandle {
    let base = PortDriverBase::new(
        name,
        1,
        PortFlags {
            can_block,
            ..PortFlags::default()
        },
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_INTERRUPTS,
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

async fn processed(db: &PvDatabase, name: &str) {
    db.process_record(name).await.unwrap();
    for _ in 0..400 {
        if !db.get_record(name).unwrap().read().is_processing() {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("{name} never completed");
}

fn alarm(db: &PvDatabase, name: &str) -> (EpicsValue, EpicsValue) {
    (
        db.get_pv(format!("{name}.STAT")).unwrap(),
        db.get_pv(format!("{name}.SEVR")).unwrap(),
    )
}

fn alarmed(stat: u16, sevr: i16) -> (EpicsValue, EpicsValue) {
    (EpicsValue::Short(stat as i16), EpicsValue::Short(sevr))
}

#[epics_base_rs::epics_test]
async fn a_write_read_writes_its_command_record_and_reads_the_reply() {
    let sync_writes = Arc::new(Mutex::new(Vec::new()));
    let blk_writes = Arc::new(Mutex::new(Vec::new()));
    let db = Arc::new(PvDatabase::new());
    let _sync = port("WRSYNC", false, &db, &sync_writes);
    let _blk = port("WRBLK", true, &db, &blk_writes);
    // CMD holds an escaped command; the record translates it each process.
    let records = r#"
        record(stringout, "CMD") { field(VAL, "ID?\\n") }
        record(ao, "NUM") { field(VAL, "2.5") field(PREC, "2") }
        record(waveform, "EMPTY") { field(FTVL, "DOUBLE") field(NELM, "4") }
        record(stringin, "SI") { field(DTYP, "asynOctetWriteRead") field(INP, "@asyn(WRSYNC,0,1)CMD") }
        record(stringin, "SI_NUM") { field(DTYP, "asynOctetWriteRead") field(INP, "@asyn(WRSYNC,0,1)NUM") }
        record(stringin, "SELF") {
            field(DTYP, "asynOctetWriteRead") field(INP, "@asyn(WRSYNC,0,1)SELF") field(VAL, "PING")
        }
        record(lsi, "LSI") { field(DTYP, "asynOctetWriteRead") field(INP, "@asyn(WRSYNC,0,1)CMD") field(SIZV, "40") }
        record(waveform, "WF") {
            field(DTYP, "asynOctetWriteRead") field(INP, "@asyn(WRBLK,0,1)CMD")
            field(FTVL, "CHAR") field(NELM, "40")
        }
        record(stringin, "SELF_B") {
            field(DTYP, "asynOctetWriteRead") field(INP, "@asyn(WRBLK,0,1)SELF_B") field(VAL, "PONG")
        }
        record(stringin, "NO_GET") { field(DTYP, "asynOctetWriteRead") field(INP, "@asyn(WRSYNC,0,1)EMPTY") }
        record(stringin, "MISSING") { field(DTYP, "asynOctetWriteRead") field(INP, "@asyn(WRSYNC,0,1)NOPE") }
    "#;
    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
        .with_database(db)
        .db_string(records, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();

    // A port that cannot block reads the command under the record's hold.
    processed(&db, "SI").await;
    assert_eq!(db.get_pv("SI").unwrap(), EpicsValue::String("ID?\n".into()));
    assert_eq!(alarm(&db, "SI"), alarmed(0, 0));
    // A number renders at its record's precision, as dbGet(DBR_STRING) does.
    processed(&db, "SI_NUM").await;
    assert_eq!(
        db.get_pv("SI_NUM").unwrap(),
        EpicsValue::String("2.50".into())
    );
    // A record that names itself sends its own value.
    processed(&db, "SELF").await;
    assert_eq!(
        db.get_pv("SELF").unwrap(),
        EpicsValue::String("PING".into())
    );
    processed(&db, "LSI").await;
    assert_eq!(db.get_pv("LSI.LEN").unwrap(), EpicsValue::ULong(5));
    assert_eq!(
        sync_writes.lock().unwrap().as_slice(),
        [&b"ID?\n"[..], b"2.50", b"PING", b"ID?\n"]
    );

    // A blocking port reads it on the port thread.
    processed(&db, "WF").await;
    assert_eq!(db.get_pv("WF.NORD").unwrap(), EpicsValue::ULong(4));
    processed(&db, "SELF_B").await;
    assert_eq!(
        db.get_pv("SELF_B").unwrap(),
        EpicsValue::String("PONG".into())
    );
    assert_eq!(
        blk_writes.lock().unwrap().as_slice(),
        [&b"ID?\n"[..], b"PONG"]
    );

    // An empty array fails dbGet (S_db_onlyOne): READ/INVALID, no I/O, VAL kept.
    let before = sync_writes.lock().unwrap().len();
    processed(&db, "NO_GET").await;
    assert_eq!(alarm(&db, "NO_GET"), alarmed(alarm_status::READ_ALARM, 3));
    assert_eq!(sync_writes.lock().unwrap().len(), before);

    // initDbAddr's missing record: dead from init. Its LINK/INVALID stays
    // pending, as C's does, behind the UDF alarm a record starts with.
    let rec = db.get_record("MISSING").unwrap();
    assert!(rec.read().is_processing());
    assert_eq!(rec.read().common.nsta, alarm_status::LINK_ALARM);
    assert_eq!(alarm(&db, "MISSING"), alarmed(alarm_status::UDF_ALARM, 3));
}
