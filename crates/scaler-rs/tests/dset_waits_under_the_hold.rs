//! C calls the dset's reset, write_preset and arm inline under `dbScanLock`,
//! and devScalerAsyn's synchronous port (drvScaler974) waits there on its
//! can-block serial port. Each such call runs inside `sync_driver_wait`, so
//! asyn's under-hold assert lets the driver's SyncIO through.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::{ProcStack, PvDatabase, in_sync_driver_wait, scan_lock_held};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset};
use epics_base_rs::types::EpicsValue;
use scaler_rs::MAX_SCALER_CHANNELS;
use scaler_rs::device_support::scaler_asyn::{
    DoneInterrupt, ScalerAsynDeviceSupport, ScalerDriver,
};

static ASYN_SCALER: DevSup = DevSup {
    name: "devScalerAsyn",
    choice: "Asyn Scaler",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(7, &[2, 4, 5, 6], false),
};

type Calls = Arc<Mutex<Vec<(&'static str, Option<bool>, bool)>>>;

/// Records, per call, whether it ran under the hold and inside the scope.
struct Recording(Calls);

impl Recording {
    fn note(&self, what: &'static str) {
        self.0
            .lock()
            .unwrap()
            .push((what, scan_lock_held(), in_sync_driver_wait()));
    }
}

impl ScalerDriver for Recording {
    fn reset(&mut self) -> epics_base_rs::error::CaResult<()> {
        self.note("reset");
        Ok(())
    }
    fn read(&mut self, _: &mut [u32; MAX_SCALER_CHANNELS]) -> epics_base_rs::error::CaResult<()> {
        Ok(())
    }
    fn write_preset(&mut self, _: usize, preset: u32) -> epics_base_rs::error::CaResult<u32> {
        self.note("preset");
        Ok(preset)
    }
    fn arm(&mut self, start: bool) -> epics_base_rs::error::CaResult<()> {
        self.note(if start { "arm" } else { "disarm" });
        Ok(())
    }
    fn register_done_interrupt(&mut self, _: DoneInterrupt) {}
    fn num_channels(&self) -> usize {
        1
    }
}

const DB: &str = r#"
record(scaler, "SC") {
    field(DTYP, "Asyn Scaler")
    field(OUT,  "@asyn(scalerPort 0 0)")
    field(FREQ, "10000000")
    field(TP,   "1")
    field(CONT, "$(CONT)")
}
"#;

async fn boot(cont: &str) -> (Arc<PvDatabase>, Calls) {
    let calls = Calls::default();
    let sink = calls.clone();
    let (name, factory) = scaler_rs::scaler_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_device_support("scaler", &ASYN_SCALER, move || {
            Box::new(ScalerAsynDeviceSupport::new(Box::new(Recording(
                sink.clone(),
            ))))
        })
        .db_string(DB, &HashMap::from([("CONT".to_string(), cont.to_string())]))
        .unwrap()
        .build()
        .await
        .unwrap();
    calls.lock().unwrap().clear();
    (db, calls)
}

fn assert_scoped(calls: &Calls, expect: &[&str]) {
    let calls = calls.lock().unwrap();
    let names: Vec<_> = calls.iter().map(|c| c.0).collect();
    assert_eq!(names, expect);
    let debug = cfg!(debug_assertions);
    for c in calls.iter() {
        assert_eq!(c.1, debug.then_some(true), "{} under the hold", c.0);
        assert_eq!(c.2, debug, "{} inside sync_driver_wait", c.0);
    }
}

#[tokio::test]
async fn a_user_count_start_and_stop_wait_inside_the_scope() {
    let (db, calls) = boot("0").await;
    db.put_record_field_from_ca("SC", "CNT", EpicsValue::Short(1))
        .await
        .unwrap();
    assert_scoped(&calls, &["reset", "preset", "arm"]);
    calls.lock().unwrap().clear();
    db.put_record_field_from_ca("SC", "CNT", EpicsValue::Short(0))
        .await
        .unwrap();
    assert_scoped(&calls, &["disarm"]);
}

#[tokio::test]
async fn an_autocount_start_waits_inside_the_scope() {
    let (db, calls) = boot("1").await;
    db.process_record_with_links("SC", &mut ProcStack::new())
        .await
        .unwrap();
    assert_scoped(&calls, &["reset", "preset", "arm"]);
}
