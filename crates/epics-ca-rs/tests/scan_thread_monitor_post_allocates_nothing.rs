//! A process cycle on `scan-0.1` posts to its monitors without allocating:
//! `db_post_events` fills a free-listed fixed `db_field_log` per monitor
//! (`dbEvent.c:700-738`, `:876-906`). Counted per thread name with this
//! binary's own allocator, as monitored minus unmonitored over the same
//! ticks, the unmonitored cycle's own cost being outside this change.

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

#[path = "common/budget.rs"]
mod budget;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::{CaChannel, CaClient, MonitorHandle};
use epics_ca_rs::protocol::{CA_PROTO_CREATE_CHAN, CA_PROTO_EVENT_ADD, CaHeader};
use epics_ca_rs::server::CaServer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SCAN_THREAD: &str = "scan-0.1";

thread_local! {
    /// 0 not yet asked, 1 `scan-0.1`, 2 any other thread. Asking may itself
    /// allocate; that reentry reads 2 and is not counted.
    static ON_SCAN: Cell<u8> = const { Cell::new(0) };
}

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static FREES: AtomicU64 = AtomicU64::new(0);

fn on_scan() -> bool {
    ON_SCAN.with(|s| match s.get() {
        0 => {
            s.set(2);
            let yes = std::thread::current().name() == Some(SCAN_THREAD);
            s.set(if yes { 1 } else { 2 });
            yes
        }
        v => v == 1,
    })
}

struct Counting;

// SAFETY: forwards every call to `System` unchanged; the thread test reads a
// const-initialised thread-local and the counters are atomics.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if on_scan() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if on_scan() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if on_scan() {
            FREES.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const N_AO: usize = 20;
/// stringin and waveform records whose VAL is monitored: a 40-byte string and
/// an array fail `useValque` (`dbEvent.c:493-500`), so each post is a
/// reference log and copies nothing.
const N_REF: usize = 20;

fn db() -> String {
    let mut db = String::from(
        r#"
record(calc, "TICK") { field(SCAN, ".1 second") field(CALC, "VAL+1") }
record(stringin, "SI") { field(SCAN, ".1 second") field(VAL, "hello") field(MPST, "Always") }
record(waveform, "WF") { field(SCAN, ".1 second") field(FTVL, "DOUBLE") field(NELM, "8") }
"#,
    );
    for i in 0..N_AO {
        db.push_str(&format!(
            "record(ao, \"AO{i}\") {{ field(SCAN, \".1 second\") field(OMSL, \"closed_loop\") field(DOL, \"TICK\") field(OUT, \"SINK{i}\") }}\n\
             record(ao, \"SINK{i}\") {{ }}\n"
        ));
    }
    for i in 0..N_REF {
        db.push_str(&format!(
            "record(stringin, \"SI{i}\") {{ field(SCAN, \".1 second\") field(VAL, \"s{i}\") field(MPST, \"Always\") }}\n\
             record(waveform, \"WF{i}\") {{ field(SCAN, \".1 second\") field(FTVL, \"DOUBLE\") field(NELM, \"8\") field(MPST, \"Always\") }}\n"
        ));
    }
    db
}

/// Allocations and frees on `scan-0.1`, and the ticks they span.
struct Window {
    allocs: u64,
    frees: u64,
    ticks: u64,
}

impl Window {
    /// Per tick, scaled by 1000 so a sub-unit difference stays visible.
    fn milli_per_tick(&self) -> (u64, u64) {
        (
            self.allocs * 1000 / self.ticks,
            self.frees * 1000 / self.ticks,
        )
    }
}

fn tick(db: &PvDatabase) -> u64 {
    match db.get_pv("TICK").expect("TICK") {
        EpicsValue::Double(v) => v as u64,
        other => panic!("TICK: {other:?}"),
    }
}

/// The counters and the tick, read in the idle gap after a sweep, so a
/// window holds whole sweeps only.
async fn between_sweeps(db: &PvDatabase) -> (u64, u64, u64) {
    let t = tick(db);
    while tick(db) == t {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
    (
        ALLOCS.load(Ordering::Relaxed),
        FREES.load(Ordering::Relaxed),
        tick(db),
    )
}

async fn window(db: &PvDatabase) -> Window {
    tokio::time::sleep(Duration::from_secs(1)).await;
    let (a0, f0, t0) = between_sweeps(db).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (a1, f1, t1) = between_sweeps(db).await;
    let w = Window {
        allocs: a1 - a0,
        frees: f1 - f0,
        ticks: t1 - t0,
    };
    assert!(w.ticks >= 10, "scan-0.1 ran {} ticks in 3 s", w.ticks);
    w
}

async fn connect(client: &CaClient, name: &str) -> CaChannel {
    let ch = client.create_channel(name);
    ch.wait_connected(budget::FACT_BUDGET)
        .await
        .unwrap_or_else(|e| panic!("{name} connects: {e:?}"));
    ch
}

async fn monitor(client: &CaClient, name: &str) -> MonitorHandle {
    connect(client, name)
        .await
        .subscribe()
        .await
        .unwrap_or_else(|e| panic!("{name} subscribes: {e:?}"))
}

/// Every value `mon` holds now.
fn drain(mon: &mut MonitorHandle) -> Vec<EpicsValue> {
    let mut got = Vec::new();
    while let Some(Some(Ok(snap))) = mon.recv().now_or_never() {
        got.push(snap.value);
    }
    got
}

trait NowOrNever: std::future::Future + Sized {
    fn now_or_never(self) -> Option<Self::Output> {
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        match std::pin::pin!(self).poll(&mut cx) {
            std::task::Poll::Ready(v) => Some(v),
            std::task::Poll::Pending => None,
        }
    }
}

impl<F: std::future::Future> NowOrNever for F {}

const DBR_CTRL_DOUBLE: u16 = 34;

/// A `DBR_CTRL_DOUBLE`, `DBE_VALUE` monitor spoken on the wire, the type the
/// client library does not subscribe with. Yields `(precision, value)`.
async fn ctrl_monitor(port: u16, name: &str) -> tokio::sync::mpsc::UnboundedReceiver<(i16, f64)> {
    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let mut version = CaHeader::new(0);
    version.count = epics_ca_rs::protocol::CA_MINOR_VERSION;
    let mut padded = name.as_bytes().to_vec();
    padded.resize((name.len() + 1).next_multiple_of(8), 0);
    let mut create = CaHeader::new(CA_PROTO_CREATE_CHAN);
    create.postsize = padded.len() as u16;
    create.cid = 7;
    create.available = epics_ca_rs::protocol::CA_MINOR_VERSION as u32;
    let mut out = version.to_bytes().to_vec();
    out.extend_from_slice(&create.to_bytes());
    out.extend_from_slice(&padded);
    sock.write_all(&out).await.expect("create");

    let mut acc = Vec::new();
    let sid = loop {
        let (hdr, body) = next_frame(&mut sock, &mut acc).await;
        drop(body);
        if hdr.cmmd == CA_PROTO_CREATE_CHAN {
            break hdr.available;
        }
    };
    let mut add = CaHeader::new(CA_PROTO_EVENT_ADD);
    add.postsize = 16;
    add.data_type = DBR_CTRL_DOUBLE;
    add.count = 1;
    add.cid = sid;
    add.available = 9;
    let mut out = add.to_bytes().to_vec();
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&[0u8; 2]);
    sock.write_all(&out).await.expect("event add");

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let (hdr, body) = next_frame(&mut sock, &mut acc).await;
            if hdr.cmmd == CA_PROTO_EVENT_ADD && hdr.available == 9 && body.len() >= 88 {
                let prec = i16::from_be_bytes([body[4], body[5]]);
                let value = f64::from_be_bytes(body[80..88].try_into().unwrap());
                if tx.send((prec, value)).is_err() {
                    return;
                }
            }
        }
    });
    rx
}

async fn next_frame(sock: &mut tokio::net::TcpStream, acc: &mut Vec<u8>) -> (CaHeader, Vec<u8>) {
    loop {
        if let Ok((hdr, size)) = CaHeader::from_bytes_extended(acc) {
            let len = size + hdr.actual_postsize();
            if acc.len() >= len {
                let body = acc[size..len].to_vec();
                acc.drain(..len);
                return (hdr, body);
            }
        }
        let mut buf = [0u8; 4096];
        let n = sock.read(&mut buf).await.expect("read");
        assert!(n > 0, "server closed the circuit");
        acc.extend_from_slice(&buf[..n]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_monitored_scan_cycle_allocates_nothing_on_its_scan_thread() {
    let server = CaServer::builder()
        .port(0)
        .db_string(&db(), &Default::default())
        .expect("load db")
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let tcp_port = server.tcp_port();
    let db = server.database().clone();
    let _h = tokio::spawn(async move { server.run().await });
    let _scan = epics_base_rs::server::scan::ScanOwner::start(db.clone());
    // SAFETY: the only test in this binary; set before `CaClient::new()`.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    db.put_pv("WF", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0].into()))
        .await
        .expect("seed WF");

    let unwatched = window(&db).await;
    // An ao driving its OUT link allocates nothing per process either.
    assert_eq!(
        unwatched.milli_per_tick(),
        (0, 0),
        "scan-0.1 (allocs, frees) x1000 per tick, nothing monitored"
    );

    let client = CaClient::new().await.expect("client");
    let mut tick_mon = monitor(&client, "TICK").await;
    let mut aos = Vec::new();
    for i in 0..N_AO {
        aos.push(monitor(&client, &format!("AO{i}")).await);
    }
    let mut wf = monitor(&client, "WF").await;
    let mut dbnd = monitor(&client, r#"AO2.VAL{"dbnd":{"d":1000}}"#).await;
    let mut ctrl = ctrl_monitor(tcp_port, "AO1").await;

    let watched = window(&db).await;
    assert_eq!(
        watched.milli_per_tick(),
        unwatched.milli_per_tick(),
        "scan-0.1 (allocs, frees) x1000 per tick, monitored vs not"
    );

    // Each client holds the right value: a scalar by value, a waveform read
    // at delivery.
    let ticks = drain(&mut tick_mon);
    assert!(ticks.len() >= 10, "TICK posted {} times", ticks.len());
    for (i, ao) in aos.iter_mut().enumerate() {
        let got = drain(ao);
        assert!(got.len() >= 10, "AO{i} posted {} times", got.len());
        assert!(matches!(got.last(), Some(EpicsValue::Double(v)) if *v > 10.0));
    }
    let got = drain(&mut wf);
    assert!(got.len() >= 10, "WF posted {} times", got.len());
    assert!(
        got.iter()
            .all(|v| *v == EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0].into())),
        "{got:?}"
    );
    // A one-per-tick ramp never leaves a 1000 deadband after its first post.
    assert_eq!(drain(&mut dbnd).len(), 1, "dbnd passes only the first post");
    let mut ctrl_posts = Vec::new();
    while let Ok(post) = ctrl.try_recv() {
        ctrl_posts.push(post);
    }
    assert!(ctrl_posts.len() >= 10, "CTRL posted {}", ctrl_posts.len());

    // The CTRL client reads PREC at delivery: a value post after the put
    // carries it.
    connect(&client, "AO1.PREC")
        .await
        .put(&EpicsValue::Short(4))
        .await
        .expect("put PREC");
    let prec = tokio::time::timeout(budget::FACT_BUDGET, async {
        loop {
            let (prec, _) = ctrl.recv().await.expect("CTRL open");
            if prec == 4 {
                return prec;
            }
        }
    })
    .await
    .expect("a CTRL value post with PREC 4");
    assert_eq!(prec, 4);

    // `arr` is a post-chain filter (`arr.c:177`), run by the event task
    // (`dbEvent.c:985`): its copy never lands on the scan thread.
    let mut arr = monitor(&client, r#"WF.VAL{"arr":{"s":0,"e":1}}"#).await;
    let with_arr = window(&db).await;
    assert_eq!(
        with_arr.milli_per_tick(),
        unwatched.milli_per_tick(),
        "scan-0.1 (allocs, frees) x1000 per tick, with an arr subscriber vs none"
    );
    let got = drain(&mut arr);
    assert!(got.len() >= 10, "arr posted {}", got.len());
    assert!(
        got.iter()
            .all(|v| *v == EpicsValue::DoubleArray(vec![1.0, 2.0].into())),
        "{got:?}"
    );

    // `dec` is a pre-chain filter: it runs on the log as posted, a reference
    // for a waveform, and copies nothing (`dbnd.c:75`, `sync.c:150`).
    let mut dec = monitor(&client, r#"WF.VAL{"dec":{"n":2}}"#).await;
    let with_dec = window(&db).await;
    assert_eq!(
        with_dec.milli_per_tick(),
        unwatched.milli_per_tick(),
        "scan-0.1 (allocs, frees) x1000 per tick, with a dec subscriber vs none"
    );
    let got = drain(&mut dec);
    assert!(got.len() >= 5, "dec posted {}", got.len());
    assert!(
        got.iter()
            .all(|v| *v == EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0].into())),
        "{got:?}"
    );

    // Twenty stringin and twenty waveform VAL monitors: each post is a
    // reference with no value, read at delivery on the server's thread.
    let mut refs = Vec::new();
    for i in 0..N_REF {
        refs.push((monitor(&client, &format!("SI{i}")).await, i, true));
        refs.push((monitor(&client, &format!("WF{i}")).await, i, false));
    }
    let with_refs = window(&db).await;
    assert_eq!(
        with_refs.milli_per_tick(),
        unwatched.milli_per_tick(),
        "scan-0.1 (allocs, frees) x1000 per tick, with {N_REF} stringin and {N_REF} waveform monitors vs none"
    );
    for (mon, i, si) in &mut refs {
        let got = drain(mon);
        assert!(got.len() >= 10, "SI/WF{i} posted {} times", got.len());
        if *si {
            assert!(
                got.iter()
                    .all(|v| *v == EpicsValue::String(format!("s{i}").into()))
            );
        }
    }

    // A stringin VAL subscriber reads the live field at delivery.
    let mut si = monitor(&client, "SI").await;
    let with_si = window(&db).await;
    eprintln!(
        "scan-0.1 (allocs, frees) x1000 per tick: unwatched {:?} watched {:?} +arr {:?} +stringin {:?}",
        unwatched.milli_per_tick(),
        watched.milli_per_tick(),
        with_arr.milli_per_tick(),
        with_si.milli_per_tick()
    );
    let got = drain(&mut si);
    assert!(got.len() >= 10, "SI posted {} times", got.len());
    assert!(got.iter().all(|v| *v == EpicsValue::String("hello".into())));
}
