//! C mca calls `recGblGetTimeStamp` only where a read begins
//! (`mcaRecord.c:762`) and where acquisition is seen stopped (`:811`), so a
//! status poll while acquiring keeps TIME.

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — runs on the exec backend
// (`EPICS_RS_BUILD_EXEC_BACKEND=thread`); no CA server is built.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::{DeviceInitOutcome, DeviceReadOutcome, DeviceSupport};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record};
use epics_base_rs::types::{EpicsValue, WallTime};
use mca_rs::record::{McaCommand, McaDset, McaRecord, McaStatus, Nack};

static ACQUIRING: AtomicBool = AtomicBool::new(true);

static BUSY_MCA: DevSup = DevSup {
    name: "devMcaBusy",
    choice: "Busy",
    link_type: DbLinkType::Constant,
    dset: Dset::new(6, &[2, 4, 5], false),
};

/// A device whose status reports [`ACQUIRING`].
struct Busy;

impl DeviceSupport for Busy {
    fn dtyp(&self) -> &str {
        "Busy"
    }
    fn init(&mut self, _record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        Ok(DeviceInitOutcome::ok())
    }
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let mca = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<McaRecord>())
            .unwrap();
        mca.run_device_cycle(self);
        Ok(DeviceReadOutcome::ok())
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}

impl McaDset for Busy {
    fn send_msg(&mut self, _command: McaCommand) -> Result<(), Nack> {
        Ok(())
    }
    fn read_status(&mut self, mca: &McaRecord) -> Result<McaStatus, Nack> {
        Ok(McaStatus {
            acquiring: ACQUIRING.load(Ordering::SeqCst),
            dwell_time: mca.dwel,
            ..Default::default()
        })
    }
    fn read_array(&mut self, mca: &mut McaRecord) -> Result<(), Nack> {
        mca.land_channel_count(mca.nuse);
        Ok(())
    }
}

fn time(db: &PvDatabase) -> WallTime {
    db.get_record("M").unwrap().read().common.time
}

async fn put(db: &PvDatabase, field: &str) {
    // Each stamp is a distinct instant.
    std::thread::sleep(Duration::from_millis(2));
    db.put_record_field_from_ca("M", field, EpicsValue::String("1".into()))
        .await
        .unwrap();
}

#[tokio::test]
async fn time_moves_only_at_its_stamp_points() {
    let (name, factory) = mca_rs::mca_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_device_support("mca", &BUSY_MCA, || Box::new(Busy))
        .db_string(
            r#"record(mca, "M") { field(DTYP, "Busy") field(NMAX, "4") field(NUSE, "4") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;

    ACQUIRING.store(true, Ordering::SeqCst);
    put(&db, "PROC").await;
    let polled = time(&db);
    put(&db, "PROC").await;
    assert_eq!(time(&db), polled, "a status poll while acquiring");

    put(&db, "READ").await;
    let read = time(&db);
    assert_ne!(read, polled, "a read begins (:762)");
    put(&db, "PROC").await;
    assert_eq!(time(&db), read, "the poll after it");

    ACQUIRING.store(false, Ordering::SeqCst);
    put(&db, "PROC").await;
    assert_ne!(time(&db), read, "acquisition seen stopped (:811)");
}
