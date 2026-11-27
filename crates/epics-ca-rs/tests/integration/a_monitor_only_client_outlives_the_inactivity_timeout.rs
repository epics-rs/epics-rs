//! With `EPICS_CAS_INACTIVITY_TMO` set, a client that only receives monitor
//! updates is not idle. libca sends nothing on a circuit that keeps
//! delivering to it (`tcpRecvWatchdog::messageArrivalNotify` restarts its echo
//! timer on every frame, `tcpiiu.cpp:528`), so the cap runs from the last byte
//! the circuit moved either way, as rsrv's `time_at_last_send` and
//! `time_at_last_recv` do (`caserverio.c:72`, `camsgtask.c:113`).
//!
//! Ports are always ephemeral (`:0`), never the real 5064.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::protocol::{
    CA_MINOR_VERSION, CA_PROTO_CREATE_CHAN, CA_PROTO_ECHO, CA_PROTO_EVENT_ADD, CA_PROTO_VERSION,
    CaHeader,
};
#[cfg(tokio_backend)]
use epics_ca_rs::server::CaServer;
use epics_ca_rs::server::blocking::BlockingCaServer;
use fact_budget::{FACT_BUDGET, barrier};

const PV: &str = "IDLE:MONITORED";
const DBR_DOUBLE: u16 = 6;
/// `EPICS_CAS_INACTIVITY_TMO`, at its 30 s floor.
const CAP: Duration = Duration::from_secs(30);
/// Between two posts of the monitored PV, well inside `CAP`.
const POST_EVERY: Duration = Duration::from_secs(1);

/// SAFETY: nextest runs each test in a process of its own, and this runs
/// before the test starts a thread or a server reads the variable.
fn set_inactivity_cap() {
    unsafe { std::env::set_var("EPICS_CAS_INACTIVITY_TMO", CAP.as_secs().to_string()) };
}

#[derive(Debug)]
enum Seen {
    Frame(u16),
    Closed,
}

/// The next frame's command, `Closed` once the server closed the circuit, or
/// `None` once `within` passes with nothing.
fn next_seen(sock: &mut TcpStream, within: Duration) -> Option<Seen> {
    sock.set_read_timeout(Some(within)).expect("read timeout");
    let mut hdr = [0u8; CaHeader::SIZE];
    match sock.read_exact(&mut hdr) {
        Ok(()) => {}
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => return None,
        Err(_) => return Some(Seen::Closed),
    }
    let mut body = vec![0u8; u16::from_be_bytes([hdr[2], hdr[3]]) as usize];
    sock.set_read_timeout(Some(FACT_BUDGET))
        .expect("read timeout");
    match sock.read_exact(&mut body) {
        Ok(()) => Some(Seen::Frame(u16::from_be_bytes([hdr[0], hdr[1]]))),
        Err(_) => Some(Seen::Closed),
    }
}

/// Reads frames until one with command `cmmd`, returning its header.
fn frame_of(sock: &mut TcpStream, cmmd: u16) -> [u8; CaHeader::SIZE] {
    sock.set_read_timeout(Some(FACT_BUDGET))
        .expect("read timeout");
    loop {
        let mut hdr = [0u8; CaHeader::SIZE];
        sock.read_exact(&mut hdr).expect("a frame from the server");
        let mut body = vec![0u8; u16::from_be_bytes([hdr[2], hdr[3]]) as usize];
        sock.read_exact(&mut body).expect("the frame's body");
        if u16::from_be_bytes([hdr[0], hdr[1]]) == cmmd {
            return hdr;
        }
    }
}

fn create_chan_frame(cid: u32, pv: &str) -> Vec<u8> {
    let name = epics_ca_rs::protocol::pad_string(pv);
    let mut h = CaHeader::new(CA_PROTO_CREATE_CHAN);
    h.postsize = name.len() as u16;
    h.cid = cid;
    h.available = CA_MINOR_VERSION as u32;
    let mut f = h.to_bytes().to_vec();
    f.extend_from_slice(&name);
    f
}

fn event_add_frame(sid: u32, subid: u32) -> Vec<u8> {
    let mut h = CaHeader::new(CA_PROTO_EVENT_ADD);
    h.data_type = DBR_DOUBLE;
    h.count = 1;
    h.postsize = 16;
    h.cid = sid;
    h.available = subid;
    let mut f = h.to_bytes().to_vec();
    f.extend_from_slice(&[0u8; 12]);
    f.extend_from_slice(&3u16.to_be_bytes()); // DBE_VALUE | DBE_LOG
    f.extend_from_slice(&[0u8; 2]);
    f
}

/// A circuit subscribed to `PV`, after its initial update; the client sends
/// nothing more on it.
fn subscribed(addr: SocketAddr) -> TcpStream {
    let mut sock = TcpStream::connect(addr).expect("connect");
    let mut version = CaHeader::new(CA_PROTO_VERSION);
    version.count = CA_MINOR_VERSION;
    let mut hello = version.to_bytes().to_vec();
    hello.extend_from_slice(&create_chan_frame(0, PV));
    sock.write_all(&hello).expect("write the handshake");
    let created = frame_of(&mut sock, CA_PROTO_CREATE_CHAN);
    let sid = u32::from_be_bytes([created[12], created[13], created[14], created[15]]);
    sock.write_all(&event_add_frame(sid, 1))
        .expect("write EVENT_ADD");
    frame_of(&mut sock, CA_PROTO_EVENT_ADD);
    sock
}

/// Stops the poster even when an assertion fails.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// A circuit that only receives updates still answers after a circuit that
/// opened later and stayed silent was closed for idling.
fn assert_monitor_only_client_kept(addr: SocketAddr, db: Arc<PvDatabase>) {
    let mut monitored = subscribed(addr);
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = StopOnDrop(stop.clone());
    let poster = thread::spawn(move || {
        let mut value = 1.0;
        while !stop.load(Ordering::Relaxed) {
            // not-a-wait: pacing between posts, well inside CAP.
            std::thread::sleep(POST_EVERY);
            block_on_sync(db.put_pv(PV, EpicsValue::Double(value)))
                .expect("no async runtime on the poster thread")
                .expect("post the monitored PV");
            value += 1.0;
        }
    });

    // Two posted updates put the silent circuit's open after the monitored
    // one's last read, so on reads alone the monitored one idles out first.
    frame_of(&mut monitored, CA_PROTO_EVENT_ADD);
    frame_of(&mut monitored, CA_PROTO_EVENT_ADD);
    let mut silent = TcpStream::connect(addr).expect("connect the silent circuit");
    frame_of(&mut silent, CA_PROTO_VERSION);
    let silent_end = next_seen(&mut silent, CAP + FACT_BUDGET);
    assert!(
        matches!(silent_end, Some(Seen::Closed)),
        "a silent circuit is closed once the cap passes, got {silent_end:?}"
    );

    monitored
        .write_all(&CaHeader::new(CA_PROTO_ECHO).to_bytes())
        .expect("write ECHO");
    barrier::until(
        "the monitored circuit's ECHO reply after the silent circuit idled out",
        |seen| matches!(seen, Seen::Closed),
        |seen| matches!(seen, Seen::Frame(CA_PROTO_ECHO)),
        |remaining| next_seen(&mut monitored, remaining),
    );

    drop(stopping);
    poster.join().expect("poster thread");
}

#[cfg(tokio_backend)]
#[test]
fn the_async_loop_keeps_a_monitor_only_client_past_the_cap() {
    set_inactivity_cap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let server_thread = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async move {
            let server = CaServer::builder()
                .port(0)
                .tcp_port(0)
                .pv(PV, EpicsValue::Double(0.0))
                .build()
                .await
                .expect("build CA server");
            ready_tx
                .send((server.tcp_port(), server.database().clone()))
                .expect("report the server");
            tokio::select! {
                _ = server.run() => {}
                _ = tokio::task::spawn_blocking(move || { let _ = stop_rx.recv(); }) => {}
            }
        });
    });

    let (port, db) = ready_rx.recv().expect("async server reports its port");
    assert_monitor_only_client_kept(SocketAddr::from(([127, 0, 0, 1], port)), db);

    let _ = stop_tx.send(());
    let _ = server_thread.join();
}

#[test]
fn the_blocking_loop_keeps_a_monitor_only_client_past_the_cap() {
    set_inactivity_cap();
    let db = Arc::new(PvDatabase::new());
    block_on_sync(db.add_pv(PV, EpicsValue::Double(0.0)))
        .expect("no async runtime on this thread")
        .expect("add_pv");
    let server = Arc::new(
        BlockingCaServer::bind(
            "127.0.0.1:0",
            db.clone(),
            epics_base_rs::server::access_security::new_acf_cell(None),
        )
        .expect("bind ephemeral port"),
    );
    let addr = server.local_addr().expect("local_addr");
    let srv = server.clone();
    let accept = thread::spawn(move || srv.serve());

    assert_monitor_only_client_kept(addr, db);

    server.shutdown();
    let _ = accept.join();
}
