//! `SoftScalerPorts::connect` against the process path's lock order.
//!
//! A scaler's process holds its record, then its driver, then the port's
//! input state. An input may itself be a record on that path — here the
//! scaler's own `S1..S8`. `connect` must therefore never wait for a record
//! while holding the input state, or a process already holding the record
//! deadlocks against it.

use std::collections::HashMap;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset};
use scaler_rs::MAX_SCALER_CHANNELS;
use scaler_rs::device_support::soft_port::SoftScalerPorts;

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
    field(OUT,  "@asyn(P 0 0)")
    field(FREQ, "10000000")
    field(TP,   "1")
}
"#;

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p
// scaler-rs --test soft_port_connect`). No CA server is built; the reactor
// comes from `#[tokio::test]`, which the backend does not remove.
#[tokio::test(flavor = "multi_thread")]
async fn connect_waits_for_an_input_record_without_holding_the_input_state() {
    let ports = Arc::new(SoftScalerPorts::default());
    ports.configure("P", 8, "SC.S%d").unwrap();
    let (name, factory) = scaler_rs::scaler_record_factory();
    let dsets = Arc::clone(&ports);
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_device_support("scaler", &ASYN_SCALER, move || {
            Box::new(dsets.device_support("P").expect("P is configured"))
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let cell = db.get_record("SC").unwrap();
    let driver = ports.device_support("P").unwrap().driver();

    // The process path: hold the scaler record, then read the driver.
    let (held, holding) = mpsc::channel();
    let (done, finished) = mpsc::channel();
    let process = {
        let done = done.clone();
        thread::spawn(move || {
            let guard = cell.write();
            held.send(()).unwrap();
            // Long enough for `connect` to reach the record.
            thread::sleep(Duration::from_millis(300));
            let mut counts = [0u32; MAX_SCALER_CHANNELS];
            let _ = driver.lock().unwrap().read(&mut counts);
            drop(guard);
            done.send("process").unwrap();
        })
    };
    holding.recv().unwrap();
    let handle = tokio::runtime::Handle::current();
    let connect = thread::spawn(move || {
        let _rt = handle.enter();
        ports.connect(&db);
        done.send("connect").unwrap();
    });

    for _ in 0..2 {
        finished
            .recv_timeout(Duration::from_secs(5))
            .expect("connect and the process path deadlocked");
    }
    process.join().unwrap();
    connect.join().unwrap();
}
