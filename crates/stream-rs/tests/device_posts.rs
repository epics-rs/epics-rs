//! A `readData`'s `db_post_events` ([`StreamIo::post_events`]) reaches the
//! record's monitors where C's does: inline in a process the device answers
//! synchronously, before the record's own `monitor()` (C holds `dbScanLock`
//! there, `StreamEpics.cc:873`), and from the bus thread of an `@init` rerun
//! (`StreamEpics.cc:446`), which C runs unlocked.

mod common;

use std::collections::HashMap;
use std::sync::Weak;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::pv::MonitorEvent;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record};
use epics_base_rs::types::EpicsValue;
use stream_rs::bus::{self, Client, ReadInput, StreamBusInterface, StreamIoStatus};
use stream_rs::epics::{
    ERROR, Format, OK, StreamDevice, StreamInitArgs, StreamIo, StreamRecordType,
};

static LONGOUT: [DevSup; 1] = [DevSup {
    name: "devlongoutStream",
    choice: "stream",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[2, 3, 4], false),
}];

/// What the buses answer every `in`.
static ANSWER: AtomicI32 = AtomicI32::new(0);

/// The bus "S" answers each request inline, as a synchronous asyn port
/// does (`asynManager.c:1555-1584`); "T" answers from a thread of its own.
struct TestBus {
    client: Weak<dyn Client>,
    inline: bool,
}

impl TestBus {
    fn answer(&self, f: impl FnOnce(&dyn Client) + Send + 'static) -> bool {
        let client = self.client.clone();
        let give = move || {
            if let Some(client) = client.upgrade() {
                f(&*client);
            }
        };
        if self.inline {
            give();
        } else {
            std::thread::spawn(give);
        }
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
    let inline = match busname {
        "S" => true,
        "T" => false,
        _ => return None,
    };
    Some(Box::new(TestBus { client, inline }))
}

/// A `readData` that posts VAL outside a process as an output's does
/// (`devaoStream.c:66-104`), with the negated value, so its post and the
/// record's own `monitor()` post differ.
fn read(io: &StreamIo<'_>, record: &mut dyn Record, _: &mut CommonFields, f: &Format<'_>) -> i64 {
    let mut val = 0;
    if io.scanf_long(f, &mut val) == ERROR {
        return ERROR;
    }
    let _ = record.put_field("VAL", EpicsValue::Long(val as i32));
    if !io.pact() {
        io.post_events("VAL", EpicsValue::Long(-(val as i32)), EventMask::VALUE);
    }
    OK
}

struct Longout;

impl StreamRecordType for Longout {
    fn init_record(&self, _record: &mut dyn Record, common: &mut CommonFields) -> StreamInitArgs {
        StreamInitArgs {
            link: common.text.out.to_string(),
            read_data: Some(read),
            write_data: None,
            map_error: false,
        }
    }

    fn is_output(&self) -> bool {
        true
    }
}

/// The events `sub` takes within a second, until `n`.
async fn events(sub: &mut DbSubscription, n: usize) -> Vec<(i32, EventMask)> {
    events_by(sub, n, long).await
}

/// [`events`], each as `f` makes it.
async fn events_by<T>(sub: &mut DbSubscription, n: usize, f: fn(&MonitorEvent) -> T) -> Vec<T> {
    let mut got = Vec::new();
    let start = Instant::now();
    while got.len() < n && start.elapsed() < Duration::from_secs(1) {
        match sub.try_recv_event() {
            Ok(e) => got.push(f(&e)),
            Err(_) => epics_base_rs::runtime::task::sleep(Duration::from_millis(2)).await,
        }
    }
    // and nothing after
    epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
    while let Ok(e) = sub.try_recv_event() {
        got.push(f(&e));
    }
    got
}

/// A scalar's value, or an array's first element.
fn long(e: &MonitorEvent) -> (i32, EventMask) {
    let v = match e.value().as_value().cloned().unwrap() {
        EpicsValue::LongArray(a) => a.as_slice()[0] as f64,
        v => v.to_f64().unwrap(),
    };
    (v as i32, e.mask())
}

async fn subscribe(db: &PvDatabase, pv: &str) -> DbSubscription {
    let mut sub = DbSubscription::subscribe(db, pv).await.unwrap();
    while sub.try_recv_event().is_ok() {}
    sub
}

async fn run() {
    common::setup();
    stream_rs::epics::register_driver();
    bus::register_stream_bus_interface("Test", test_bus);
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("device_posts");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("dp.proto"),
        "readback { out \"W\"; in \"%d\"; wait 200; @init { out \"I\"; in \"%d\"; } }\n\
         mismatch { out \"W\"; in \"X%d\"; @init { out \"I\"; in \"%d\"; } }\n",
    )
    .unwrap();
    // SAFETY: set before any thread of this test binary reads it.
    unsafe { std::env::set_var("STREAM_PROTOCOL_PATH", &dir) };

    ANSWER.store(5, Ordering::SeqCst);
    let db_text = "record(longout, \"sync\") { field(DTYP, \"stream\") field(OUT, \"@dp.proto readback S\") }\n\
                   record(longout, \"unlocked\") { field(DTYP, \"stream\") field(OUT, \"@dp.proto readback T\") }\n\
                   record(ao, \"rbv\") { field(DTYP, \"stream\") field(OUT, \"@dp.proto readback T\") field(HIGH, \"8\") field(HSV, \"MINOR\") }\n\
                   record(aao, \"arr\") { field(DTYP, \"stream\") field(OUT, \"@dp.proto mismatch T\") field(FTVL, \"LONG\") field(NELM, \"2\") field(MPST, \"On Change\") }\n\
                   record(aao, \"arr2\") { field(DTYP, \"stream\") field(OUT, \"@dp.proto readback T\") field(FTVL, \"LONG\") field(NELM, \"2\") }\n";
    let ioc = IocBuilder::new()
        .register_dynamic_device_support(&[("longout", &LONGOUT)], |ctx: &DeviceSupportContext| {
            (ctx.record_type, ctx.dtyp)
                .eq(&("longout", "stream"))
                .then(|| Box::new(StreamDevice::new(Longout)) as _)
        })
        .register_dynamic_device_support(
            &[(
                "ao",
                std::slice::from_ref(&stream_rs::dev::ao::DEV_AO_STREAM),
            )],
            |ctx: &DeviceSupportContext| {
                (ctx.record_type, ctx.dtyp)
                    .eq(&("ao", "stream"))
                    .then(|| Box::new(stream_rs::dev::ao::device()) as _)
            },
        )
        .register_dynamic_device_support(
            &[(
                "aao",
                std::slice::from_ref(&stream_rs::dev::aao::DEV_AAO_STREAM),
            )],
            |ctx: &DeviceSupportContext| {
                (ctx.record_type, ctx.dtyp)
                    .eq(&("aao", "stream"))
                    .then(|| Box::new(stream_rs::dev::aao::device()) as _)
            },
        )
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);

    // Locked: the post is out when the process that made it returns, the
    // `wait` still running, and the @init run's post (-5, made at iocInit)
    // went to nobody. The record's own monitor follows at the completion
    // (with no `wait` C's would never come: a protocol that finishes inline
    // asks for no completion, `StreamEpics.cc:1038`, then `:885` sets PACT).
    let mut sub = subscribe(&db, "sync").await;
    ANSWER.store(7, Ordering::SeqCst);
    db.put_record_field_from_ca_no_notify("sync", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
    let mut got = Vec::new();
    while let Ok(e) = sub.try_recv_event() {
        got.push(long(&e));
    }
    assert_eq!(got, vec![(-7, EventMask::VALUE)]);
    let got = events(&mut sub, 1).await;
    assert_eq!(got, vec![(7, EventMask::VALUE | EventMask::LOG)]);

    // An aao readback in a process posts nothing itself (`devaaoStream.c:147`,
    // `if (record->pact) return OK`): the record's monitor() posts VAL once
    // the `wait` ends.
    let mut arr2 = subscribe(&db, "arr2").await;
    db.put_record_field_from_ca_no_notify("arr2", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    assert!(arr2.try_recv_event().is_err());
    let got = events(&mut arr2, 1).await;
    assert_eq!(got, vec![(7, EventMask::VALUE | EventMask::LOG)]);

    // arr's process fails its input match: INVALID.
    let mut arr_sevr = subscribe(&db, "arr.SEVR").await;
    db.put_record_field_from_ca_no_notify("arr", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
    let got = events(&mut arr_sevr, 1).await;
    assert_eq!(got, vec![(3, EventMask::VALUE)]);
    let mut arr = subscribe(&db, "arr").await;
    let mut arr_alarm = DbSubscription::subscribe_with_mask(&db, "arr", 0, EventMask::ALARM.bits())
        .await
        .unwrap();
    while arr_alarm.try_recv_event().is_ok() {}
    let mut arr_hash = subscribe(&db, "arr.HASH").await;

    // rbv goes MINOR (VAL 10 > HIGH) with RBV 9 read back.
    let mut sevr = subscribe(&db, "rbv.SEVR").await;
    ANSWER.store(9, Ordering::SeqCst);
    db.put_record_field_from_ca_no_notify("rbv", "VAL", EpicsValue::Double(10.0))
        .await
        .unwrap();
    let got = events(&mut sevr, 1).await;
    assert_eq!(got, vec![(1, EventMask::VALUE)]);

    // Unlocked: the @init rerun's posts, from the bus thread, masks as C's.
    let mut sub = subscribe(&db, "unlocked").await;
    // devaoStream.c: RBV goes 9 to 3 with (int)VAL 3, which C's
    // `orbv = (epicsInt32)(oval = val)` (`:73`) did not post.
    let mut rbv = subscribe(&db, "rbv.RBV").await;
    ANSWER.store(3, Ordering::SeqCst);
    assert_eq!(epics_base_rs::server::ioc_app::ioc_pause(), 0);
    assert_eq!(epics_base_rs::server::ioc_app::ioc_run(), 0);
    let got = events(&mut sub, 1).await;
    assert_eq!(got, vec![(-3, EventMask::VALUE)]);
    let got = events(&mut rbv, 1).await;
    assert_eq!(got, vec![(3, EventMask::VALUE | EventMask::LOG)]);
    // recGblResetAlarms' SEVR post, DBE_VALUE (`recGbl.c:204`)
    let got = events(&mut sevr, 1).await;
    assert_eq!(got, vec![(0, EventMask::VALUE)]);
    // devaaoStream.c:149-174: the alarm reset's SEVR, HASH on its change
    // (MPST On Change), then VAL with the reset's DBE_ALARM, MPST's
    // DBE_VALUE and APST Always's DBE_LOG (a subscriber sees its own mask's
    // part).
    let got = events(&mut arr_sevr, 1).await;
    assert_eq!(got, vec![(0, EventMask::VALUE)]);
    let got = events(&mut arr_hash, 1).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1, EventMask::VALUE);
    // NORD's element, of NELM's two
    let got = events_by(&mut arr, 1, |e| {
        let v = e.value().as_value().cloned().unwrap();
        (v, e.mask())
    })
    .await;
    assert_eq!(
        got,
        vec![(
            EpicsValue::LongArray(vec![3].into()),
            EventMask::VALUE | EventMask::LOG
        )]
    );
    let got = events(&mut arr_alarm, 1).await;
    assert_eq!(got, vec![(3, EventMask::ALARM)]);
}

#[test]
fn a_device_post_reaches_monitors_where_c_posts_it() {
    epics_base_rs::runtime::task::test_block_on(run());
}
