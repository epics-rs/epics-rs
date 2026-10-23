//! devint64outStream.c's `@init` post (`readData`, devint64outStream.c:51-65):
//! the mask is recGblResetAlarms' DBE_ALARM plus DBE_VALUE / DBE_LOG by
//! MDEL / ADEL, and nothing is posted when the mask is empty.

mod common;

use std::collections::HashMap;
use std::sync::Weak;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_app::{ioc_pause, ioc_run};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::EpicsValue;
use stream_rs::bus::{self, Client, ReadInput, StreamBusInterface, StreamIoStatus};

/// What the bus answers every `in`.
static ANSWER: AtomicI64 = AtomicI64::new(0);

/// Answers each request from a thread of its own.
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
    (busname == "B").then(|| Box::new(TestBus { client }) as _)
}

async fn subscribe(db: &PvDatabase, pv: &str) -> DbSubscription {
    let mask = EventMask::VALUE | EventMask::LOG | EventMask::ALARM;
    let mut sub = DbSubscription::subscribe_with_mask(db, pv, 0, mask.bits())
        .await
        .unwrap();
    while sub.try_recv_event().is_ok() {}
    sub
}

/// The events each of `subs` took by now.
async fn drain(subs: &mut [DbSubscription]) -> Vec<Vec<(i64, EventMask)>> {
    epics_base_rs::runtime::task::sleep(Duration::from_millis(300)).await;
    subs.iter_mut()
        .map(|sub| {
            let mut got = Vec::new();
            while let Ok(e) = sub.try_recv_event() {
                let v = match e.value().as_value() {
                    Some(EpicsValue::Int64(v)) => *v,
                    v => v.and_then(|v| v.to_f64()).unwrap() as i64,
                };
                got.push((v, e.mask()));
            }
            got
        })
        .collect()
}

/// The `@init` rerun of iocRun, every record reading `answer`.
async fn rerun(answer: i64, subs: &mut [DbSubscription]) -> Vec<Vec<(i64, EventMask)>> {
    ANSWER.store(answer, Ordering::SeqCst);
    assert_eq!(ioc_pause(), 0);
    assert_eq!(ioc_run(), 0);
    drain(subs).await
}

async fn run() {
    common::setup();
    bus::register_stream_bus_interface("Test", test_bus);
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("dev_int64out");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("lo.proto"),
        "lo { out \"W%d\"; in \"%d\"; @init { out \"I\"; in \"%d\"; } }\n",
    )
    .unwrap();
    // SAFETY: set before any thread of this test binary reads it.
    unsafe { std::env::set_var("STREAM_PROTOCOL_PATH", &dir) };

    ANSWER.store(5, Ordering::SeqCst);
    let rec = |name: &str, more: &str| {
        format!(
            "record(int64out, \"{name}\") {{ field(DTYP, \"stream\") field(OUT, \"@lo.proto lo B\") {more} }}\n"
        )
    };
    let db_text = rec("m", "")
        + &rec("d", "field(MDEL, \"10\") field(ADEL, \"-1\")")
        + &rec("a", "field(HIHI, \"50\") field(HHSV, \"MAJOR\")");
    let ioc = stream_rs::dev::register(IocBuilder::new())
        .db_string(&db_text, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    let mut subs = vec![
        subscribe(&db, "m").await,
        subscribe(&db, "d").await,
        subscribe(&db, "a").await,
        subscribe(&db, "a.SEVR").await,
    ];
    let (v, l, a) = (EventMask::VALUE, EventMask::LOG, EventMask::ALARM);

    // Nothing moved: no post at all; ADEL -1 still posts DBE_LOG.
    assert_eq!(
        rerun(5, &mut subs).await,
        vec![vec![], vec![(5, l)], vec![], vec![]]
    );
    // Within MDEL 10: DBE_LOG only; then MLST and ALST are 14.
    assert_eq!(
        rerun(14, &mut subs).await,
        vec![vec![(14, v | l)], vec![(14, l)], vec![(14, v | l)], vec![]]
    );
    assert_eq!(
        rerun(14, &mut subs).await,
        vec![vec![], vec![(14, l)], vec![], vec![]]
    );
    // Past MDEL, and from INT64_MAX to INT64_MIN: C's epicsInt64 DELTA wraps
    // to -1 there (devint64outStream.c:27) and posts nothing.
    let max = i64::MAX;
    assert_eq!(
        rerun(max, &mut subs).await,
        vec![
            vec![(max, v | l)],
            vec![(max, v | l)],
            vec![(max, v | l)],
            vec![]
        ]
    );
    let min = i64::MIN;
    assert_eq!(
        rerun(min, &mut subs).await,
        vec![
            vec![(min, v | l)],
            vec![(min, v | l)],
            vec![(min, v | l)],
            vec![]
        ]
    );

    // A MAJOR alarm from the readback past HIHI, the put's 10 under it:
    // readData leaves it to the record (PACT), whose monitor() posts it.
    ANSWER.store(100, Ordering::SeqCst);
    db.put_record_field_from_ca_no_notify("a", "VAL", EpicsValue::Int64(10))
        .await
        .unwrap();
    let got = drain(&mut subs).await;
    assert_eq!(got[2..], [vec![(100, v | l | a)], vec![(2, v)]]);
    // Cleared by the rerun's recGblResetAlarms: DBE_ALARM alone.
    assert_eq!(
        rerun(100, &mut subs).await,
        vec![
            vec![(100, v | l)],
            vec![(100, v | l)],
            vec![(100, a)],
            vec![(0, v)]
        ]
    );
}

#[test]
fn int64out_init_post_mask() {
    epics_base_rs::runtime::task::test_block_on(run());
}
