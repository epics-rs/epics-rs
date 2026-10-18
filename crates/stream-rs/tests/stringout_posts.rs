//! devstringoutStream.c's `readData` posts against C StreamDevice's: the
//! run of `oracle/stringout/run-posts.sh`, here on a bus that answers as its
//! `dev.py` does, with the monitors its camonitors take, compared with
//! `posts.log`.

mod common;

use std::collections::HashMap;
use std::sync::Weak;
use std::time::Duration;

use parking_lot::Mutex;

use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::pv::MonitorEvent;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::types::EpicsValue;
use stream_rs::bus::{self, Client, ReadInput, StreamBusInterface, StreamIoStatus};

/// dev.py's replies to the `@init` query, counted per record.
const REPLIES: [&str; 4] = ["v0", "v1", "v1", "v2"];

/// dev.py on an asyn IP port: every answer comes from another thread, the
/// reply to "Q<x>" is the next of [`REPLIES`] (one bus per record, so
/// counted per x) and any other request gets none, which asyn reports once
/// ReplyTimeout is over.
struct Dev {
    client: Weak<dyn Client>,
    last: Mutex<Vec<u8>>,
    queries: Mutex<usize>,
}

impl Dev {
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

impl StreamBusInterface for Dev {
    fn lock_request(&self, _: u64) -> bool {
        self.answer(|c| c.lock_callback(StreamIoStatus::StreamIoSuccess))
    }
    fn unlock(&self) -> bool {
        true
    }
    fn write_request(&self, output: &[u8], _: u64) -> bool {
        *self.last.lock() = output.to_vec();
        self.answer(|c| c.write_callback(StreamIoStatus::StreamIoSuccess))
    }
    fn read_request(&self, _: u64, _: u64, _: isize, _: bool) -> bool {
        if !self.last.lock().starts_with(b"Q") {
            return self.answer(|c| {
                c.read_callback(StreamIoStatus::StreamIoNoReply, ReadInput::Bytes(b""));
            });
        }
        // one reply per query
        self.last.lock().clear();
        let mut n = self.queries.lock();
        let reply = format!("{}\r\n", REPLIES[(*n).min(REPLIES.len() - 1)]);
        *n += 1;
        self.answer(move |c| {
            c.read_callback(
                StreamIoStatus::StreamIoEnd,
                ReadInput::Bytes(reply.as_bytes()),
            );
        })
    }
}

fn dev(
    client: Weak<dyn Client>,
    busname: &str,
    _: i32,
    _: &str,
) -> Option<Box<dyn StreamBusInterface>> {
    (busname == "P").then(|| {
        Box::new(Dev {
            client,
            last: Mutex::new(Vec::new()),
            queries: Mutex::new(0),
        }) as _
    })
}

fn oracle_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/oracle/stringout")
}

/// A camonitor line's `value stat sevr`: the alarm columns empty for none.
fn shown(e: &MonitorEvent) -> String {
    let value = match e.value().as_value() {
        Some(EpicsValue::String(s)) => s.to_string(),
        Some(v) => v.to_f64().unwrap().to_string(),
        None => panic!("an event with no value"),
    };
    match e.sevr() {
        0 => format!("{value}  "),
        s => format!(
            "{value} {} {}",
            alarm_condition_string(e.stat()),
            ["NO_ALARM", "MINOR", "MAJOR", "INVALID"][s as usize]
        ),
    }
}

async fn run() -> Vec<String> {
    bus::register_stream_bus_interface("Dev", dev);
    common::setup();
    // SAFETY: set before any thread of this test binary reads it.
    unsafe { std::env::set_var("STREAM_PROTOCOL_PATH", oracle_dir()) };

    let db_text = r#"
record(stringout, "a") { field(DTYP, "stream") field(OUT, "@posts.proto p(a) P") field(VAL, "x") }
record(stringout, "b") { field(DTYP, "stream") field(OUT, "@posts.proto p(b) P") field(VAL, "x") field(MPST, "Always") field(APST, "Always") }
record(stringout, "c") { field(DTYP, "stream") field(OUT, "@posts.proto p(c) P") field(VAL, "x") field(MPST, "Always") }
record(stringout, "d") { field(DTYP, "stream") field(OUT, "@posts.proto q(d) P") field(VAL, "x") }
"#;
    let ioc = stream_rs::dev::register(IocBuilder::new())
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);

    let watched = [
        (
            "v",
            EventMask::VALUE,
            &[
                "a", "b", "c", "d", "a.SEVR", "b.SEVR", "c.SEVR", "a.STAT", "b.STAT", "c.STAT",
            ][..],
        ),
        ("l", EventMask::LOG, &["a", "b", "c", "d"][..]),
        ("a", EventMask::ALARM, &["a", "b", "c", "d"][..]),
    ];
    let mut subs = Vec::new();
    for (m, mask, pvs) in watched {
        for pv in pvs {
            let mut sub = DbSubscription::subscribe_with_mask(&db, pv, 0, mask.bits())
                .await
                .unwrap();
            // the connect event
            while sub.try_recv_event().is_ok() {}
            subs.push((m, *pv, sub));
        }
    }

    // A string event's value is read as it is taken (`dbfl_type_ref`), so
    // each step's events are taken before the next step changes VAL.
    let mut got = Vec::new();
    let mut take = |subs: &mut Vec<(&str, &str, DbSubscription)>| {
        for (m, pv, sub) in subs {
            while let Ok(e) = sub.try_recv_event() {
                got.push(format!("{m} {pv} {}", shown(&e)));
            }
        }
    };
    let pause = |ms| epics_base_rs::runtime::task::sleep(Duration::from_millis(ms));
    for r in ["a", "b", "c", "d"] {
        db.put_record_field_from_ca_no_notify(r, "PROC", EpicsValue::Long(1))
            .await
            .unwrap();
    }
    pause(1500).await;
    take(&mut subs);
    for _ in 0..3 {
        assert_eq!(epics_base_rs::server::ioc_app::ioc_pause(), 0);
        assert_eq!(epics_base_rs::server::ioc_app::ioc_run(), 0);
        pause(1000).await;
        take(&mut subs);
    }

    got.sort_by(|x, y| {
        let key = |s: &String| s.splitn(3, ' ').take(2).collect::<Vec<_>>().join(" ");
        key(x).cmp(&key(y))
    });
    got
}

#[test]
fn readdata_posts_as_c_does() {
    let got = epics_base_rs::runtime::task::test_block_on(run());
    let want: Vec<String> = std::fs::read_to_string(oracle_dir().join("posts.log"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(got, want);
}
