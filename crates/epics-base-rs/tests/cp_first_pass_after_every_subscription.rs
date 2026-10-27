//! At `dbCaRun` the dbCa worker services every link's `CA_MONITOR_NATIVE`
//! before any `CA_DBPROCESS`: each `CA_CONNECT` re-queues its link's monitor
//! at the work list's tail, and a first event's process is queued behind those
//! (`dbCa.c:1209-1236`, `:1312-1319`). A holder with two CP inputs therefore
//! never processes with its second link still unsubscribed; on the AD IOCs
//! `PoolUsedBuffers` latched INVALID in ACKS when it did.

mod calink_ioc;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::runtime::task::{CallbackPriority, spawn_blocking_background};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record};
use epics_base_rs::types::EpicsValue;

static GATED: DevSup = DevSup {
    name: "devAiGated",
    choice: "Gated",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], false),
};

/// An I/O Intr source whose reads leave VAL 1, each one first waiting for
/// `release` when it has one.
struct Gated {
    intr: Option<mpsc::Receiver<()>>,
    release: Option<Mutex<std::sync::mpsc::Receiver<()>>>,
}

impl DeviceSupport for Gated {
    fn dtyp(&self) -> &str {
        "Gated"
    }
    fn io_intr_receiver(&mut self) -> Option<mpsc::Receiver<()>> {
        self.intr.take()
    }
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        if let Some(release) = &self.release {
            let _ = release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10));
        }
        record.put_field("VAL", EpicsValue::Double(1.0))?;
        Ok(DeviceReadOutcome::computed(DeviceUdf::Untouched))
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// Boundary — the second input's source has a read in flight when `dbCaRun`
/// releases the links, so its link subscribes after the first link's event:
/// the holder's first pass still reads both inputs.
#[epics_macros_rs::epics_test]
async fn a_holders_first_pass_waits_for_its_second_link() {
    let (pulse_a, intr_a) = mpsc::channel(4);
    let (pulse_b, intr_b) = mpsc::channel(4);
    let (release, release_rx) = std::sync::mpsc::channel();
    let devices = Mutex::new(vec![
        Gated {
            intr: Some(intr_b),
            release: Some(Mutex::new(release_rx)),
        },
        Gated {
            intr: Some(intr_a),
            release: None,
        },
    ]);
    let (db, _) = IocBuilder::new()
        .register_device_support("ai", &GATED, move || {
            Box::new(devices.lock().unwrap().pop().expect("two Gated records"))
        })
        .db_string(
            r#"
record(ai, "SRC_A") { field(DTYP, "Gated") field(INP, "@a") field(SCAN, "I/O Intr") }
record(ai, "SRC_B") { field(DTYP, "Gated") field(INP, "@b") field(SCAN, "I/O Intr") }
record(calc, "HOLD") {
    field(INPA, "SRC_A CP MS") field(INPB, "SRC_B CP MS") field(CALC, "A+B")
}
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    calink_ioc::until("the links open", || db.external_link_opens_completed() >= 2).await;
    db.scan_control().run();
    pulse_a.try_send(()).unwrap();
    calink_ioc::until("SRC_A processes", || num(&db, "SRC_A.UDF") == 0.0).await;
    // SRC_B's read is in flight until released, so its link's turn waits.
    pulse_b.try_send(()).unwrap();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        release.send(()).unwrap();
    });
    db.process_cp_links_on_connect().await;
    db.ca_sync().await;
    releaser.join().unwrap();

    assert_eq!(num(&db, "HOLD"), 2.0, "the holder read both sources");
    assert_eq!(
        (num(&db, "HOLD.SEVR"), num(&db, "HOLD.ACKS")),
        (0.0, 0.0),
        "no pass read SRC_B's link unsubscribed"
    );
}

/// Wait up to the fact budget for `pv` to read `want`.
async fn reads_within(db: &PvDatabase, pv: &str, want: f64) -> bool {
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while num(db, pv) != want {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    true
}

/// Boundary — a link opened before `dbCaRun` whose monitor is refused
/// (`ca_add_array_event` failing, `dbCa.c:1230-1233`; here the per-PV
/// subscriber cap): it never has a first event, and holds no other link's
/// process for one.
#[epics_macros_rs::epics_test]
async fn a_refused_subscription_holds_no_first_pass() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"
record(ai, "FULL") { field(VAL, "1") }
record(ai, "SRC") { field(VAL, "5") }
record(calc, "REFUSED") { field(INPA, "FULL CP") field(CALC, "A") }
record(calc, "HOLD") { field(INPA, "SRC CP") field(CALC, "A") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    calink_ioc::until("the links open", || db.external_link_opens_completed() >= 2).await;
    let mut full = Vec::new();
    while let Ok(sub) =
        epics_base_rs::server::database::db_access::DbSubscription::subscribe(&db, "FULL").await
    {
        full.push(sub);
    }
    db.scan_control().run();
    let ca_run = epics_base_rs::runtime::task::timeout(
        Duration::from_secs(5),
        db.process_cp_links_on_connect(),
    )
    .await;

    assert!(ca_run.is_ok(), "dbCaRun waited on the refused link");
    assert!(
        reads_within(&db, "HOLD", 5.0).await,
        "HOLD's first pass waited on the refused link"
    );
}

/// Boundary — a link opened after `dbCaRun` whose first event waits on its
/// source's pump, the interrupt queued there not yet processing: C's
/// `dbCaTask` adds the monitor at once (`dbCa.c:1225-1229`), so the link
/// holds no other link's process.
#[epics_macros_rs::epics_test]
async fn a_link_opened_after_ca_run_holds_no_process() {
    let (pulse, intr) = mpsc::channel(4);
    let device = Mutex::new(Some(Gated {
        intr: Some(intr),
        release: None,
    }));
    let (db, _) = IocBuilder::new()
        .register_device_support("ai", &GATED, move || {
            Box::new(device.lock().unwrap().take().expect("one Gated record"))
        })
        .db_string(
            r#"
record(ai, "BUSY") {
    field(DTYP, "Gated") field(INP, "@b") field(SCAN, "I/O Intr") field(PRIO, "HIGH")
}
record(ai, "SRC") { field(VAL, "5") }
record(calc, "HOLD") { field(INPA, "SRC CP") field(CALC, "A") }
record(calc, "LATE") { field(CALC, "A") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;
    calink_ioc::until("HOLD's first pass", || num(&db, "HOLD") == 5.0).await;
    // BUSY's pump runs on the held HIGH band, so its interrupt stays queued
    // with BUSY unlocked, and LATE's first event waits for the pump's turn.
    let (release, held) = std::sync::mpsc::channel::<()>();
    let (busy, is_busy) = std::sync::mpsc::channel();
    let hold = spawn_blocking_background(CallbackPriority::High, move |_| {
        busy.send(()).unwrap();
        let _ = held.recv();
    });
    is_busy.recv_timeout(fact_budget::FACT_BUDGET).unwrap();
    pulse.try_send(()).unwrap();
    let opened = db.external_link_opens_completed();
    db.put_record_field_from_ca("LATE", "INPA", EpicsValue::String("BUSY CP".into()))
        .await
        .unwrap();
    calink_ioc::until("LATE's link opens", || {
        db.external_link_opens_completed() > opened
    })
    .await;
    db.put_record_field_from_ca("SRC", "VAL", EpicsValue::Double(7.0))
        .await
        .unwrap();
    let processed = reads_within(&db, "HOLD", 7.0).await;
    let _ = release.send(());
    let _ = hold.await;

    assert!(processed, "HOLD's process waited on LATE's first event");
    assert!(
        reads_within(&db, "LATE", 1.0).await,
        "LATE's first event read BUSY once its pump ran"
    );
}
