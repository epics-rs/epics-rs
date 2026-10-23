//! devcalcoutStream.c's `readData` posts VAL, and `recGblResetAlarms` its
//! alarm, from the bus thread of an `@init` rerun, with C's monitor mask
//! but for the ADEL test C gets wrong.

mod common;

use std::collections::HashMap;
use std::sync::Weak;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::pv::MonitorEvent;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::EpicsValue;
use stream_rs::bus::{self, Client, ReadInput, StreamBusInterface, StreamIoStatus};

/// What the bus answers every `in`.
static ANSWER: AtomicI32 = AtomicI32::new(0);

/// Answers each request from a thread of its own, as an asynchronous asyn
/// port does.
struct TestBus {
    client: Weak<dyn Client>,
}

impl TestBus {
    fn answer(&self, f: impl FnOnce(&dyn Client) + Send + 'static) -> bool {
        let client = self.client.clone();
        std::thread::spawn(move || {
            if let Some(client) = client.upgrade() {
                f(&*client);
            }
        });
        true
    }
}

impl StreamBusInterface for TestBus {
    fn lock_request(&self, _: u64) -> bool {
        self.answer(|c| c.lock_callback(StreamIoStatus::StreamIoSuccess))
    }
    fn unlock(&self) -> bool {
        true
    }
    fn write_request(&self, _: &[u8], _: u64) -> bool {
        self.answer(|c| c.write_callback(StreamIoStatus::StreamIoSuccess))
    }
    fn read_request(&self, _: u64, _: u64, _: isize, _: bool) -> bool {
        self.answer(|c| {
            let input = ANSWER.load(Ordering::SeqCst).to_string();
            c.read_callback(
                StreamIoStatus::StreamIoEnd,
                ReadInput::Bytes(input.as_bytes()),
            );
        })
    }
}

fn test_bus(
    client: Weak<dyn Client>,
    busname: &str,
    _: i32,
    _: &str,
) -> Option<Box<dyn StreamBusInterface>> {
    (busname == "T").then(|| Box::new(TestBus { client }) as _)
}

/// The events `sub` takes within a second, until `n`, and any after.
async fn events(sub: &mut DbSubscription, n: usize) -> Vec<(f64, EventMask)> {
    let mut got = Vec::new();
    let start = Instant::now();
    while got.len() < n && start.elapsed() < Duration::from_secs(1) {
        match sub.try_recv_event() {
            Ok(e) => got.push(value(&e)),
            Err(_) => epics_base_rs::runtime::task::sleep(Duration::from_millis(2)).await,
        }
    }
    epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
    while let Ok(e) = sub.try_recv_event() {
        got.push(value(&e));
    }
    got
}

fn value(e: &MonitorEvent) -> (f64, EventMask) {
    let v = e.value().as_value().and_then(|v| v.to_f64()).unwrap();
    (v, e.mask())
}

/// A monitor of `pv` on DBE_VALUE, DBE_LOG and DBE_ALARM.
async fn subscribe(db: &PvDatabase, pv: &str) -> DbSubscription {
    let mask = EventMask::VALUE | EventMask::LOG | EventMask::ALARM;
    let mut sub = DbSubscription::subscribe_with_mask(db, pv, 0, mask.bits())
        .await
        .unwrap();
    while sub.try_recv_event().is_ok() {}
    sub
}

fn field(db: &PvDatabase, name: &str) -> Option<EpicsValue> {
    let (rec, name) = name.split_once('.').unwrap_or(("co", name));
    db.get_record(rec).unwrap().read().record.get_field(name)
}

async fn run() {
    common::setup();
    bus::register_stream_bus_interface("Test", test_bus);
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("calcout_posts");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("cp.proto"),
        "w { out \"W%f\"; @init { out \"I\"; in \"%d\"; } }\n\
         rb { out \"W%f\"; in \"%d\"; }\n",
    )
    .unwrap();
    // SAFETY: set before any thread of this test binary reads it.
    unsafe { std::env::set_var("STREAM_PROTOCOL_PATH", &dir) };

    ANSWER.store(5, Ordering::SeqCst);
    let db_text = "record(calcout, \"co\") { field(DTYP, \"stream\") field(OUT, \"@cp.proto w T\") \
                   field(CALC, \"A\") field(A, \"10\") field(HIGH, \"8\") field(HSV, \"MINOR\") }\n\
                   record(calcout, \"rb\") { field(DTYP, \"stream\") field(OUT, \"@cp.proto rb T\") \
                   field(CALC, \"A\") field(A, \"10\") field(HIGH, \"8\") field(HSV, \"MINOR\") }\n";
    let ioc = stream_rs::dev::register(IocBuilder::new())
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    assert_eq!(field(&db, "VAL"), Some(EpicsValue::Double(5.0)));

    // co goes MINOR (VAL 10 > HIGH).
    let mut sevr = subscribe(&db, "co.SEVR").await;
    db.put_record_field_from_ca_no_notify("co", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
    let got = events(&mut sevr, 1).await;
    assert_eq!(got, vec![(1.0, EventMask::VALUE)]);
    assert_eq!(field(&db, "ALST"), Some(EpicsValue::Double(10.0)));

    // The @init rerun reads 9: the alarm reset and the VAL change post
    // together. C's ADEL test reads the MLST its MDEL test just set and
    // leaves out DBE_LOG (`devcalcoutStream.c:62`).
    let mut val = subscribe(&db, "co").await;
    ANSWER.store(9, Ordering::SeqCst);
    assert_eq!(epics_base_rs::server::ioc_app::ioc_pause(), 0);
    assert_eq!(epics_base_rs::server::ioc_app::ioc_run(), 0);
    let got = events(&mut val, 1).await;
    assert_eq!(
        got,
        vec![(9.0, EventMask::VALUE | EventMask::LOG | EventMask::ALARM)]
    );
    // recGblResetAlarms' SEVR post, DBE_VALUE (`recGbl.c:204`)
    let got = events(&mut sevr, 1).await;
    assert_eq!(got, vec![(0.0, EventMask::VALUE)]);
    assert_eq!(field(&db, "MLST"), Some(EpicsValue::Double(9.0)));
    assert_eq!(field(&db, "ALST"), Some(EpicsValue::Double(9.0)));

    // A read inside a process posts nothing (`devcalcoutStream.c:54`): the
    // record's monitor() posts the read-back VAL with the alarm the first
    // pass raised for VAL 10.
    let mut val = subscribe(&db, "rb").await;
    let mut sevr = subscribe(&db, "rb.SEVR").await;
    ANSWER.store(7, Ordering::SeqCst);
    db.put_record_field_from_ca_no_notify("rb", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
    let got = events(&mut val, 1).await;
    assert_eq!(
        got,
        vec![(7.0, EventMask::VALUE | EventMask::LOG | EventMask::ALARM)]
    );
    let got = events(&mut sevr, 1).await;
    assert_eq!(got, vec![(1.0, EventMask::VALUE)]);
    assert_eq!(field(&db, "rb.OVAL"), Some(EpicsValue::Double(10.0)));
}

#[test]
fn an_init_rerun_posts_val_with_the_reset_mask() {
    epics_base_rs::runtime::task::test_block_on(run());
}
