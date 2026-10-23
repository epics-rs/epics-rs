//! devboStream.c's `readData` posts from an `@init` run against C
//! StreamDevice's: the steps of `oracle/bo/run-posts.sh`, each `@init` rerun
//! by iocPause and iocRun, and the masks each field's monitors took.

mod common;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Weak;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_app::{ioc_pause, ioc_run};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;
use stream_rs::bus::{self, Client, ReadInput, StreamBusInterface, StreamIoStatus};

/// run-posts.sh's device: each "I" takes the next reply, the rest "x".
static REPLIES: Mutex<VecDeque<&str>> = Mutex::new(VecDeque::new());
/// The "I" requests answered.
static ANSWERED: AtomicUsize = AtomicUsize::new(0);

/// A bus answering from a thread of its own, as an asyn IP port does.
struct Device {
    client: Weak<dyn Client>,
    request: Mutex<Vec<u8>>,
}

impl Device {
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

impl StreamBusInterface for Device {
    fn lock_request(&self, _: u64) -> bool {
        self.answer(|c| c.lock_callback(StreamIoStatus::StreamIoSuccess))
    }
    fn unlock(&self) -> bool {
        true
    }
    fn write_request(&self, output: &[u8], _: u64) -> bool {
        *self.request.lock() = output.to_vec();
        self.answer(|c| c.write_callback(StreamIoStatus::StreamIoSuccess))
    }
    fn read_request(&self, _: u64, _: u64, _: isize, _: bool) -> bool {
        let init = self.request.lock().as_slice() == b"I\r\n";
        let reply = if init {
            REPLIES.lock().pop_front().unwrap()
        } else {
            "x"
        };
        self.answer(move |c| {
            c.read_callback(
                StreamIoStatus::StreamIoSuccess,
                ReadInput::Bytes(format!("{reply}\r\n").as_bytes()),
            );
            if init {
                ANSWERED.fetch_add(1, Ordering::SeqCst);
            }
        })
    }
}

fn device(
    client: Weak<dyn Client>,
    busname: &str,
    _: i32,
    _: &str,
) -> Option<Box<dyn StreamBusInterface>> {
    (busname == "P").then(|| {
        Box::new(Device {
            client,
            request: Mutex::new(Vec::new()),
        }) as _
    })
}

const FIELDS: [&str; 4] = ["VAL", "RVAL", "RBV", "SEVR"];
const MASKS: [(&str, EventMask); 3] = [
    ("v", EventMask::VALUE),
    ("l", EventMask::LOG),
    ("a", EventMask::ALARM),
];

/// The events each field's monitor took so far, `(value, mask)`.
fn drain(subs: &mut [DbSubscription]) -> Vec<Vec<(i64, EventMask)>> {
    subs.iter_mut()
        .map(|sub| {
            let mut got = Vec::new();
            while let Ok(e) = sub.try_recv_event() {
                let v = e.value().as_value().and_then(|v| v.to_f64()).unwrap();
                got.push((v as i64, e.mask()));
            }
            got
        })
        .collect()
}

/// Rerun @init and give run-posts.sh's lines for `step`.
async fn rerun(step: &str, subs: &mut [DbSubscription]) -> Vec<String> {
    let n = ANSWERED.load(Ordering::SeqCst);
    assert_eq!(ioc_pause(), 0);
    assert_eq!(ioc_run(), 0);
    let start = Instant::now();
    while ANSWERED.load(Ordering::SeqCst) == n && start.elapsed() < Duration::from_secs(2) {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(2)).await;
    }
    assert!(ANSWERED.load(Ordering::SeqCst) > n, "{step}: no @init run");
    // the posts follow the readData that made them
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    let mut lines = Vec::new();
    for (field, got) in FIELDS.iter().zip(drain(subs)) {
        for (m, mask) in MASKS {
            let vals: Vec<String> = got
                .iter()
                .filter(|(_, e)| e.intersects(mask))
                .map(|(v, _)| v.to_string())
                .collect();
            if !vals.is_empty() {
                lines.push(format!("{step} {field} {m} {}", vals.join(" ")));
            }
        }
    }
    lines
}

/// `dbpf rec.PROC 1`: the "x" reply fails it INVALID.
async fn fail(db: &PvDatabase, subs: &mut [DbSubscription]) {
    db.put_record_field_from_ca_no_notify("rec", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
    let start = Instant::now();
    loop {
        let rec = db.get_record("rec").unwrap();
        let done = {
            let r = rec.read();
            r.common.sevr == AlarmSeverity::Invalid && !r.is_processing()
        };
        if done || start.elapsed() > Duration::from_secs(2) {
            assert!(done, "the process did not fail");
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(2)).await;
    }
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    drain(subs);
}

async fn run() {
    // ahead of common's Mock, which takes every bus name
    bus::register_stream_bus_interface("BoPosts", device);
    common::setup();
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/oracle/bo");
    // SAFETY: set before any thread of this test binary reads it.
    unsafe { std::env::set_var("STREAM_PROTOCOL_PATH", &dir) };
    REPLIES.lock().extend(["5", "7", "0", "0", "0", "3"]);

    let db_text = "record(bo, \"rec\") { field(DTYP, \"stream\") field(OUT, \"@posts.proto pi P\") field(ZNAM, \"zero\") field(ONAM, \"one\") }\n";
    let ioc = stream_rs::dev::register(IocBuilder::new())
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);

    let all = (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits();
    let mut subs = Vec::new();
    for f in FIELDS {
        let pv = format!("rec.{f}");
        subs.push(
            DbSubscription::subscribe_with_mask(&db, pv, 0, all)
                .await
                .unwrap(),
        );
    }
    drain(&mut subs);

    let mut got = Vec::new();
    for step in ["A", "B", "C"] {
        got.extend(rerun(step, &mut subs).await);
    }
    for step in ["D", "E"] {
        fail(&db, &mut subs).await;
        got.extend(rerun(step, &mut subs).await);
    }
    let want = std::fs::read_to_string(dir.join("posts.log")).unwrap();
    assert_eq!(got, want.lines().collect::<Vec<_>>());
}

#[test]
fn an_init_run_posts_with_cs_masks() {
    epics_base_rs::runtime::task::test_block_on(run());
}
