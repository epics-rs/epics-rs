//! devAsynOctet's `writeIt` on a driver that sends fewer bytes than asked:
//!
//! ```c
//! if(nbytes != nbytesTransferred) {
//!     asynPrint(..., "%s %s::%s requested %lu but sent %lu bytes\n", ...);
//!     recGblSetSevr(precord, WRITE_ALARM, MINOR_ALARM);
//!     return asynError;
//! }
//! ```
//!
//! (devAsynOctet.c:680-686). The result's status stays success, so the record
//! completes defined at WRITE/MINOR, and a command-response's read is never
//! made (:853-854). rs printed the line and raised nothing.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{PortRuntimeHandle, RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::types::EpicsValue;

/// Sends one byte short of any buffer that starts with `short`; answers every
/// read with `REPLY`.
struct ShortWriter {
    base: PortDriverBase,
    reads: Arc<AtomicUsize>,
}

impl PortDriver for ShortWriter {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        Ok(data.len() - usize::from(data.starts_with(b"short")))
    }
    fn read_octet(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        buf[..5].copy_from_slice(b"REPLY");
        Ok(5)
    }
}

fn port(
    name: &str,
    can_block: bool,
    db: &PvDatabase,
    reads: &Arc<AtomicUsize>,
) -> PortRuntimeHandle {
    let mut base = PortDriverBase::new(
        name,
        1,
        PortFlags {
            can_block,
            ..PortFlags::default()
        },
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_ALL,
    );
    base.create_param("MSG", ParamType::Octet).unwrap();
    let driver = ShortWriter {
        base,
        reads: Arc::clone(reads),
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

async fn idle(db: &PvDatabase, name: &str) {
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

fn write_minor() -> (EpicsValue, EpicsValue) {
    (
        EpicsValue::Short(alarm_status::WRITE_ALARM as i16),
        EpicsValue::Short(1),
    )
}

fn no_alarm() -> (EpicsValue, EpicsValue) {
    (EpicsValue::Short(0), EpicsValue::Short(0))
}

#[epics_base_rs::epics_test]
async fn a_short_octet_write_raises_write_minor() {
    let reads = Arc::new(AtomicUsize::new(0));
    let db = Arc::new(PvDatabase::new());
    let _sync = port("SHORTSYNC", false, &db, &reads);
    let _blk = port("SHORTBLK", true, &db, &reads);
    let records = r#"
        record(stringout, "SO_S") { field(DTYP, "asynOctetWrite") field(OUT, "@asyn(SHORTSYNC,0,1)MSG") }
        record(stringout, "SO_B") { field(DTYP, "asynOctetWrite") field(OUT, "@asyn(SHORTBLK,0,1)MSG") }
        record(waveform, "WF") {
            field(DTYP, "asynOctetWrite") field(INP, "@asyn(SHORTSYNC,0,1)MSG")
            field(FTVL, "CHAR") field(NELM, "40")
        }
        record(stringin, "CR_SHORT") { field(DTYP, "asynOctetCmdResponse") field(INP, "@asyn(SHORTSYNC,0,1)short") }
        record(stringin, "CR_FULL") { field(DTYP, "asynOctetCmdResponse") field(INP, "@asyn(SHORTSYNC,0,1)full") }
    "#;
    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
        .with_database(db)
        .db_string(records, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();

    // A port that cannot block writes inside the process; a blocking one
    // completes on the port's callback.
    for name in ["SO_S", "SO_B"] {
        for (put, want) in [("shorted", write_minor()), ("full", no_alarm())] {
            db.put_record_field_from_ca(name, "VAL", EpicsValue::String(put.into()))
                .await
                .unwrap();
            idle(&db, name).await;
            assert_eq!(alarm(&db, name), want, "{name} {put}");
        }
    }

    for (put, want) in [("shorted", write_minor()), ("full", no_alarm())] {
        db.put_record_field_from_ca(
            "WF",
            "VAL",
            EpicsValue::CharArray(put.as_bytes().to_vec().into()),
        )
        .await
        .unwrap();
        assert_eq!(alarm(&db, "WF"), want, "WF {put}");
    }

    // The short command is never answered: no read, VAL untouched, and the
    // record is defined all the same.
    db.process_record("CR_SHORT").await.unwrap();
    assert_eq!(alarm(&db, "CR_SHORT"), write_minor());
    assert_eq!(
        db.get_pv("CR_SHORT").unwrap(),
        EpicsValue::String("".into())
    );
    assert_eq!(db.get_pv("CR_SHORT.UDF").unwrap(), EpicsValue::UChar(0));
    assert_eq!(reads.load(Ordering::SeqCst), 0);

    db.process_record("CR_FULL").await.unwrap();
    assert_eq!(alarm(&db, "CR_FULL"), no_alarm());
    assert_eq!(
        db.get_pv("CR_FULL").unwrap(),
        EpicsValue::String("REPLY".into())
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}
