//! A driver built as an `asynPortDriver` with `setIntegerParam(scalerDone, 1)`
//! in its constructor (drvScaler974.cpp:89, drvUSBCTR.cpp:417) has its
//! `SCALER_DONE` flagged when `iocInit` registers the record's interrupt. The
//! port's `callbackThread` delivers it once `interruptAccept` is set
//! (asynPortDriver.cpp:923-937), `devScalerAsyn`'s `interruptCallback` raises
//! `done`, and `deviceCallbackFunc` processes the record: UDF clears, TIME is
//! stamped and the FLNK fires with no client touching the IOC. A
//! `drvScalerSoft` port has no such thread and leaves the record unprocessed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::runtime::general_time::epics_epoch;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset};
use scaler_rs::MAX_SCALER_CHANNELS;
use scaler_rs::device_support::scaler_asyn::{
    DoneInterrupt, DoneParam, ScalerAsynDeviceSupport, ScalerDriver,
};

// RTEMS-EXEC-MODEL-ALLOW(5): checked, not waived — all 5 ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p
// scaler-rs --test done_param_boot_replay`). No CA server is built; the reactor
// comes from `#[tokio::test]`, which the backend does not remove.

/// `scalerSupport.dbd`'s `device(scaler,INST_IO,devScalerAsyn,"Asyn Scaler")`.
static ASYN_SCALER: DevSup = DevSup {
    name: "devScalerAsyn",
    choice: "Asyn Scaler",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(7, &[2, 4, 5, 6], false),
};

const DB: &str = r#"
record(scaler, "SC") {
    field(DTYP, "Asyn Scaler")
    field(OUT,  "@asyn(S0 0 0)")
    field(FREQ, "10000000")
    field(FLNK, "SCF")
}
record(calc, "SCF") {
    field(CALC, "A+1")
    field(INPA, "SCF")
}
"#;

/// A port whose only state is its `SCALER_DONE` parameter.
struct ParamDriver(DoneParam);

impl ScalerDriver for ParamDriver {
    fn reset(&mut self) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    fn read(&mut self, _: &mut [u32; MAX_SCALER_CHANNELS]) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    fn write_preset(&mut self, _: usize, preset: u32) -> epics_base_rs::error::CaResult<u32> {
        Ok(preset)
    }
    fn arm(&mut self, _: bool) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    fn register_done_interrupt(&mut self, user: DoneInterrupt) {
        self.0.register(user);
    }
    fn num_channels(&self) -> usize {
        1
    }
}

async fn boot(done: DoneParam) -> Arc<PvDatabase> {
    let (name, factory) = scaler_rs::scaler_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_device_support("scaler", &ASYN_SCALER, move || {
            Box::new(ScalerAsynDeviceSupport::new(Box::new(ParamDriver(
                done.clone(),
            ))))
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

async fn field(db: &PvDatabase, rec: &str, field: &str) -> f64 {
    db.get_pv(format!("{rec}.{field}"))
        .unwrap()
        .to_f64()
        .unwrap()
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// TIME, UDF, SEVR and the FLNK target, the things the boot process changes.
async fn state(db: &PvDatabase) -> (bool, f64, f64, f64) {
    let stamped = db.get_record("SC").unwrap().read().common.time > epics_epoch();
    (
        stamped,
        field(db, "SC", "UDF").await,
        field(db, "SC", "SEVR").await,
        field(db, "SCF", "VAL").await,
    )
}

const UNPROCESSED: (bool, f64, f64, f64) = (false, 1.0, 3.0, 0.0);
const PROCESSED: (bool, f64, f64, f64) = (true, 0.0, 0.0, 1.0);

#[tokio::test(flavor = "multi_thread")]
async fn a_done_flagged_at_construction_processes_the_record_at_iocrun() {
    let db = boot(DoneParam::new(1)).await;
    settle().await;
    assert_eq!(
        state(&db).await,
        UNPROCESSED,
        "nothing before interruptAccept"
    );
    db.scan_control().run();
    settle().await;
    assert_eq!(state(&db).await, PROCESSED);
}

/// `callParamCallbacks` before `interruptAccept` keeps the flag
/// (asynPortDriver.cpp:838), so the replay still delivers it.
#[tokio::test(flavor = "multi_thread")]
async fn a_callback_before_interrupt_accept_keeps_the_flag() {
    let done = DoneParam::new(1);
    let db = boot(done.clone()).await;
    done.call_callbacks();
    db.scan_control().run();
    settle().await;
    assert_eq!(state(&db).await, PROCESSED);
}

/// The interrupt callback ignores a 0 (devScalerAsyn.c:396-397): a port
/// constructed with `scalerDone = 0` replays it and nothing processes.
#[tokio::test(flavor = "multi_thread")]
async fn a_done_of_zero_at_construction_processes_nothing() {
    let db = boot(DoneParam::new(0)).await;
    db.scan_control().run();
    settle().await;
    assert_eq!(state(&db).await, UNPROCESSED);
}

/// The replay delivers a changed value once; a pause and resume with
/// nothing changed delivers nothing, and a change after it processes again.
#[tokio::test(flavor = "multi_thread")]
async fn the_replay_is_once_and_a_later_change_processes_again() {
    let done = DoneParam::new(1);
    let db = boot(done.clone()).await;
    db.scan_control().run();
    settle().await;
    db.scan_control().pause();
    db.scan_control().run();
    settle().await;
    assert_eq!(state(&db).await, PROCESSED, "SCF processed once");

    done.set(0);
    done.call_callbacks();
    done.set(1);
    done.call_callbacks();
    settle().await;
    assert_eq!(field(&db, "SCF", "VAL").await, 2.0);
}

/// An unchanged `set` flags nothing (`paramVal::setInteger`).
#[tokio::test(flavor = "multi_thread")]
async fn an_unchanged_done_is_not_called_back() {
    let done = DoneParam::new(1);
    let db = boot(done.clone()).await;
    db.scan_control().run();
    settle().await;
    done.set(1);
    done.call_callbacks();
    settle().await;
    assert_eq!(field(&db, "SCF", "VAL").await, 1.0);
}
