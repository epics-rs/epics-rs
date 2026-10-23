//! devmbboDirectStream.c's `@init` posts (`readData`, devmbboDirectStream.c:44-79):
//! VAL with recGblResetAlarms' DBE_ALARM plus DBE_VALUE | DBE_LOG when MLST
//! moved, RVAL / RBV and each bit that moved with that mask plus DBE_VALUE |
//! DBE_LOG, and each bit that did not with VAL's mask when it is not empty.

mod common;

use std::collections::HashMap;
use std::sync::Weak;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_app::{ioc_pause, ioc_run};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use stream_rs::bus::{self, Client, ReadInput, StreamBusInterface, StreamIoStatus};

/// What the bus answers every `in`.
static ANSWER: AtomicI32 = AtomicI32::new(0);

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
async fn drain(subs: &mut [DbSubscription]) -> Vec<Vec<(i32, EventMask)>> {
    epics_base_rs::runtime::task::sleep(Duration::from_millis(300)).await;
    subs.iter_mut()
        .map(|sub| {
            let mut got = Vec::new();
            while let Ok(e) = sub.try_recv_event() {
                let v = e.value().as_value().and_then(|v| v.to_f64()).unwrap();
                got.push((v as i32, e.mask()));
            }
            got
        })
        .collect()
}

/// The `@init` rerun of iocRun, every record reading `answer`.
async fn rerun(answer: i32, subs: &mut [DbSubscription]) -> Vec<Vec<(i32, EventMask)>> {
    ANSWER.store(answer, Ordering::SeqCst);
    assert_eq!(ioc_pause(), 0);
    assert_eq!(ioc_run(), 0);
    drain(subs).await
}

async fn run() {
    common::setup();
    bus::register_stream_bus_interface("Test", test_bus);
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("dev_mbbo_direct");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("md.proto"),
        "md { out \"W%d\"; @init { out \"I\"; in \"%d\"; } }\n",
    )
    .unwrap();
    // SAFETY: set before any thread of this test binary reads it.
    unsafe { std::env::set_var("STREAM_PROTOCOL_PATH", &dir) };

    ANSWER.store(1, Ordering::SeqCst);
    let rec = |name: &str, more: &str| {
        format!(
            "record(mbboDirect, \"{name}\") {{ field(DTYP, \"stream\") field(OUT, \"@md.proto md B\") {more} }}\n"
        )
    };
    let db_text = rec("d", "") + &rec("s", "field(SHFT, \"4\")");
    let ioc = stream_rs::dev::register(IocBuilder::new())
        .db_string(&db_text, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    let mut subs = vec![
        subscribe(&db, "d").await,
        subscribe(&db, "d.RVAL").await,
        subscribe(&db, "d.B0").await,
        subscribe(&db, "d.B1").await,
        subscribe(&db, "s").await,
        subscribe(&db, "s.RBV").await,
    ];
    let (v, l) = (EventMask::VALUE, EventMask::LOG);

    // Nothing moved: no post at all.
    assert_eq!(
        rerun(1, &mut subs).await,
        vec![vec![], vec![], vec![], vec![], vec![], vec![]]
    );
    // Both bits move; s's RBV moves under SHFT 4, its VAL stays 0.
    assert_eq!(
        rerun(2, &mut subs).await,
        vec![
            vec![(2, v | l)],
            vec![(2, v | l)],
            vec![(0, v | l)],
            vec![(1, v | l)],
            vec![],
            vec![(2, v | l)]
        ]
    );
    // B1 stays, posted with VAL's mask.
    assert_eq!(
        rerun(3, &mut subs).await,
        vec![
            vec![(3, v | l)],
            vec![(3, v | l)],
            vec![(1, v | l)],
            vec![(1, v | l)],
            vec![],
            vec![(3, v | l)]
        ]
    );
    // ORAW and ORBV took the last read.
    assert_eq!(
        rerun(3, &mut subs).await,
        vec![vec![], vec![], vec![], vec![], vec![], vec![]]
    );
}

#[test]
fn mbbo_direct_init_posts() {
    epics_base_rs::runtime::task::test_block_on(run());
}
