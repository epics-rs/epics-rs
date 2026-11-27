//! C `iocBuild` calls `callbackInit` once (`iocInit.c:150-151`), and only a
//! second call warns (`callback.c:292-295`). A device-link put that wires an
//! I/O Intr record on a database before its build starts the callback bands
//! the record's processing needs, and is not that second call.

#![cfg(unix)]

use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::runtime::task::background_init;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record, register_device_object_row};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;

static WOKEN: DevSup = DevSup {
    name: "devAiPreInitWoken",
    choice: "PreInitWoken",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], true),
};

struct Woken(Arc<Mutex<Option<mpsc::Sender<()>>>>);

impl DeviceSupport for Woken {
    fn io_intr_receiver(&mut self) -> Option<mpsc::Receiver<()>> {
        let (tx, rx) = mpsc::channel(4);
        *self.0.lock().unwrap() = Some(tx);
        Some(rx)
    }

    fn io_intr_scan_independent(&self) -> bool {
        true
    }

    fn io_intr_processes_directly(&self) -> bool {
        true
    }

    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "PreInitWoken"
    }
}

/// Everything written to fd 2 while `f` runs.
async fn console_of<T>(f: impl std::future::Future<Output = T>) -> (T, String) {
    use std::os::fd::AsRawFd;
    let sink = tempfile::NamedTempFile::new().expect("capture file");
    let saved = unsafe { libc::dup(2) };
    assert!(saved >= 0, "dup(2) failed");
    assert!(
        unsafe { libc::dup2(sink.as_file().as_raw_fd(), 2) } >= 0,
        "dup2 onto fd 2 failed"
    );
    let out = f.await;
    assert!(unsafe { libc::dup2(saved, 2) } >= 0, "restore fd 2 failed");
    unsafe { libc::close(saved) };
    let printed = std::fs::read_to_string(sink.path()).expect("read the capture");
    (out, printed)
}

const CALLED_AGAIN: &str = "callbackInit called again";

#[epics_base_rs::epics_test]
async fn ioc_init_after_a_wired_pump_is_silent_and_a_repeat_callback_init_warns() {
    register_device_object_row("ai", &WOKEN);
    let wakes = Arc::new(Mutex::new(None));
    let db = Arc::new(PvDatabase::new());
    let made = wakes.clone();
    db.install_device_support_resolver(Arc::new(move |ctx| {
        (ctx.dtyp == "PreInitWoken")
            .then(|| Box::new(Woken(made.clone())) as Box<dyn DeviceSupport>)
    }));
    db.add_record("R", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    for (field, value) in [
        ("SCAN", "I/O Intr"),
        ("DTYP", "PreInitWoken"),
        ("INP", "@a"),
    ] {
        db.put_record_field_from_ca_no_notify("R", field, EpicsValue::String(value.into()))
            .await
            .unwrap_or_else(|e| panic!("R.{field}: {e}"));
    }
    assert!(
        wakes.lock().unwrap().is_some(),
        "the INP put wired R's I/O Intr"
    );

    let (built, ioc_init) =
        console_of(IocBuilder::new().with_database(db.clone()).build_ioc()).await;
    built.expect("iocInit");
    assert!(
        !ioc_init.contains(CALLED_AGAIN),
        "iocInit's callbackInit warned:\n{ioc_init}"
    );

    let ((), repeat) = console_of(async { background_init() }).await;
    assert!(
        repeat.contains(CALLED_AGAIN),
        "a second callbackInit was silent:\n{repeat}"
    );
}
