//! **A CA `DBR_CHAR` write widens UNSIGNED; a `DBR_CHAR` read stays signed.**
//!
//! C keeps two type maps and they disagree on exactly one row. The put map,
//! `dbChannel_put` (`db/db_access.c:820-...`), sends `oldDBR_CHAR` — and
//! `oldDBR_STS_CHAR`, `oldDBR_TIME_CHAR`, `oldDBR_GR_CHAR`,
//! `oldDBR_CTRL_CHAR`, and `mapOldType` (`:988`) on the WRITE_NOTIFY path —
//! to `DBR_UCHAR`, so the widening row it reaches is `putUcharLong`
//! (`dbConvert.c`, `PUT` body `*pdst = (typeb) *psrc`) and byte 0xC8 becomes
//! 200. The signed `putCharLong` is unreachable from CA. The get map keeps
//! `oldDBR_CHAR` as `DBR_CHAR` (`db_access.c:816`), so reading a `DBF_CHAR`
//! field as `DBR_LONG` still sign-extends and 0xC8 reads back as −56.
//!
//! The port had flattened that asymmetry to signed in both directions, so
//! `caput -S X:WF '\310\311'` (`caput.c`'s `charArrAsStr` branch sends
//! `DBR_CHAR`) landed −56 −55 where a C IOC lands 200 201.
//!
//! Every case drives BOTH receive loops — the async host driver
//! (`server::tcp::handle_client`) and the blocking driver the RTEMS and
//! VxWorks images run (`server::blocking`) — because they share one write
//! head (`serve_write_head`) and a test that proved only one of them is what
//! lets a shared-gate defect survive a review round.
//!
//! Ports are always ephemeral (`:0`) — never the real 5064, per the
//! `build() ⟹ listening` port-ownership rule.

// Host/tokio-only: the async server's listener stack needs a tokio reactor,
// which the `rtems-exec-model` background executor does not start. The
// comparison between the two loops is only meaningful where both can run.
#![cfg(not(feature = "rtems-exec-model"))]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_ca_rs::protocol::{
    CA_MINOR_VERSION, CA_PROTO_CREATE_CHAN, CA_PROTO_READ_NOTIFY, CA_PROTO_VERSION, CA_PROTO_WRITE,
    CaHeader,
};
use epics_ca_rs::server::CaServer;
use epics_ca_rs::server::blocking::BlockingCaServer;

/// `X:WF` is the reviewer's trigger record: a `LONG` waveform written with
/// raw bytes. `X:BYTES` is its `DBF_CHAR` twin, which pins the get direction.
const DB: &str = r#"
record(waveform,"X:WF")    { field(FTVL,"LONG") field(NELM,"8") }
record(waveform,"X:BYTES") { field(FTVL,"CHAR") field(NELM,"8") }
"#;

const DBR_CHAR: u16 = 4;
const DBR_LONG: u16 = 5;
/// `dbr_sts_char` (`db_access.h:218-223`): status(2) severity(2)
/// `dbr_char_t RISC_pad`(1) then the value — the compound shape C also maps
/// to `DBR_UCHAR`.
const DBR_STS_CHAR: u16 = 11;
const STS_CHAR_META: usize = 5;

/// The two bytes the reviewer's `caput -S X:WF '\310\311'` puts on the wire.
const WIRE_BYTES: [u8; 2] = [0xC8, 0xC9];
/// What a C IOC stores for them (`putUcharLong`).
const UNSIGNED: [i32; 2] = [200, 201];
/// What sign-extension would store instead (`putCharLong`, unreachable from CA).
const SIGNED: [i32; 2] = [-56, -55];

/// How long a read waits before the circuit is declared silent.
const READ_TIMEOUT: Duration = Duration::from_millis(2000);

/// Which driver a script ran against — only used to label assertion failures.
#[derive(Clone, Copy, Debug)]
enum Loops {
    Async,
    Blocking,
}

impl std::fmt::Display for Loops {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Async => f.write_str("async (server::tcp::handle_client)"),
            Self::Blocking => f.write_str("blocking (server::blocking)"),
        }
    }
}

// ---------------------------------------------------------------------------
// A raw client that reads whole frames
// ---------------------------------------------------------------------------

struct Raw {
    sock: TcpStream,
}

impl Raw {
    fn connect(addr: SocketAddr) -> Self {
        let sock = TcpStream::connect(addr).expect("connect");
        sock.set_read_timeout(Some(READ_TIMEOUT)).expect("timeout");
        Self { sock }
    }

    fn send(&mut self, frame: &[u8]) {
        self.sock.write_all(frame).expect("write frame");
    }

    /// Read one whole frame, header plus declared body.
    fn next_frame(&mut self) -> Vec<u8> {
        let mut hdr = [0u8; CaHeader::SIZE];
        self.sock.read_exact(&mut hdr).expect("frame header");
        let mut frame = hdr.to_vec();
        let mut body = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
        if body == 0xFFFF {
            let mut annex = [0u8; 8];
            self.sock.read_exact(&mut annex).expect("extended annex");
            frame.extend_from_slice(&annex);
            body = u32::from_be_bytes([annex[0], annex[1], annex[2], annex[3]]) as usize;
        }
        if body > 0 {
            let mut rest = vec![0u8; body];
            self.sock.read_exact(&mut rest).expect("frame body");
            frame.extend_from_slice(&rest);
        }
        frame
    }

    /// Read past the frames this conversation does not care about
    /// (`CA_PROTO_ACCESS_RIGHTS`, the server's own `CA_PROTO_VERSION`).
    fn read_until(&mut self, cmmd: u16) -> Vec<u8> {
        for _ in 0..8 {
            let frame = self.next_frame();
            if u16::from_be_bytes([frame[0], frame[1]]) == cmmd {
                return frame;
            }
        }
        panic!("no {cmmd} frame within 8 frames");
    }
}

// ---------------------------------------------------------------------------
// Frame builders
// ---------------------------------------------------------------------------

fn version_frame() -> Vec<u8> {
    let mut h = CaHeader::new(CA_PROTO_VERSION);
    h.count = CA_MINOR_VERSION;
    h.available = 0;
    h.to_bytes().to_vec()
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

/// A deprecated fire-and-forget `CA_PROTO_WRITE` carrying `payload` under
/// `data_type`, padded to CA's 8-byte message alignment.
fn write_frame(sid: u32, data_type: u16, count: u32, payload: &[u8]) -> Vec<u8> {
    let padded = payload.len().div_ceil(8) * 8;
    let mut h = CaHeader::new(CA_PROTO_WRITE);
    h.data_type = data_type;
    h.count = count as u16;
    h.cid = sid;
    h.available = 0;
    h.set_payload_size(padded, count, CA_MINOR_VERSION)
        .expect("modern peer");
    let mut f = h.to_bytes().to_vec();
    f.extend_from_slice(payload);
    f.extend(std::iter::repeat_n(0u8, padded - payload.len()));
    f
}

fn read_notify_frame(sid: u32, ioid: u32, data_type: u16, count: u32) -> Vec<u8> {
    let mut h = CaHeader::new(CA_PROTO_READ_NOTIFY);
    h.data_type = data_type;
    h.count = count as u16;
    h.cid = sid;
    h.available = ioid;
    h.set_payload_size(0, count, CA_MINOR_VERSION)
        .expect("modern peer");
    h.to_bytes().to_vec()
}

/// The `DBR_LONG` elements of a `READ_NOTIFY` reply.
fn read_notify_longs(frame: &[u8], count: usize) -> Vec<i32> {
    (0..count)
        .map(|i| {
            let off = CaHeader::SIZE + i * 4;
            i32::from_be_bytes([frame[off], frame[off + 1], frame[off + 2], frame[off + 3]])
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Fixtures: the same database in front of each loop
// ---------------------------------------------------------------------------

fn seed_db() -> Arc<PvDatabase> {
    let (db, _) = block_on_sync(
        IocBuilder::new()
            .db_string(DB, &HashMap::new())
            .expect("parse db")
            .build(),
    )
    .expect("no async runtime on this thread")
    .expect("build ioc");
    db
}

/// Create a channel on `pv`, play `write`, then read `count` elements back as
/// `DBR_LONG`.
fn write_then_read(
    addr: SocketAddr,
    pv: &str,
    write: &dyn Fn(u32) -> Vec<u8>,
    count: u32,
) -> Vec<i32> {
    let mut c = Raw::connect(addr);
    c.send(&version_frame());
    c.send(&create_chan_frame(0x2A, pv));
    let created = c.read_until(CA_PROTO_CREATE_CHAN);
    let sid = u32::from_be_bytes([created[12], created[13], created[14], created[15]]);

    c.send(&write(sid));
    c.send(&read_notify_frame(sid, 0x99, DBR_LONG, count));
    let reply = c.read_until(CA_PROTO_READ_NOTIFY);
    read_notify_longs(&reply, count as usize)
}

fn against_blocking(pv: &str, write: &dyn Fn(u32) -> Vec<u8>, count: u32) -> Vec<i32> {
    let server = Arc::new(
        BlockingCaServer::bind(
            "127.0.0.1:0",
            seed_db(),
            epics_base_rs::server::access_security::new_acf_cell(None),
        )
        .expect("bind ephemeral port"),
    );
    let addr = server.local_addr().expect("local_addr");
    let srv = server.clone();
    let accept = thread::spawn(move || srv.serve());

    let got = write_then_read(addr, pv, write, count);

    server.shutdown();
    let _ = accept.join();
    got
}

fn against_async(pv: &str, write: &dyn Fn(u32) -> Vec<u8>, count: u32) -> Vec<i32> {
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let server_thread = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async move {
            let server = CaServer::builder()
                .port(0)
                .tcp_port(0)
                .db_string(DB, &HashMap::new())
                .expect("parse db")
                .build()
                .await
                .expect("build CA server");
            port_tx.send(server.tcp_port()).expect("report tcp port");
            tokio::select! {
                _ = server.run() => {}
                _ = tokio::task::spawn_blocking(move || { let _ = stop_rx.recv(); }) => {}
            }
        });
    });

    let port = port_rx.recv().expect("async server reports its port");
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let got = write_then_read(addr, pv, write, count);

    let _ = stop_tx.send(());
    let _ = server_thread.join();
    got
}

/// Run one script against both loops and assert each landed `expect`.
fn both_loops_land(pv: &str, write: &dyn Fn(u32) -> Vec<u8>, count: u32, expect: &[i32]) {
    let blocking = against_blocking(pv, write, count);
    let asynchronous = against_async(pv, write, count);
    // Name every loop that got it wrong, not just the first: a failure that
    // stopped at the blocking driver would leave the async driver's answer
    // unstated, which is the reporting hole this file exists to close.
    let wrong: Vec<String> = [(Loops::Blocking, &blocking), (Loops::Async, &asynchronous)]
        .into_iter()
        .filter(|(_, got)| got.as_slice() != expect)
        .map(|(which, got)| format!("{which} landed {got:?}"))
        .collect();
    assert!(
        wrong.is_empty(),
        "expected {expect:?} from C's dbChannel_put map; {}",
        wrong.join("; ")
    );
    assert_eq!(
        blocking, asynchronous,
        "the two CA receive loops disagree — the put-direction type map is not shared"
    );
}

// ---------------------------------------------------------------------------
// The put direction: every CHAR shape widens unsigned
// ---------------------------------------------------------------------------

/// The bare `oldDBR_CHAR` arm of `dbChannel_put`.
#[test]
fn a_bare_dbr_char_write_widens_unsigned_on_both_loops() {
    both_loops_land(
        "X:WF",
        &|sid| write_frame(sid, DBR_CHAR, 2, &WIRE_BYTES),
        2,
        &UNSIGNED,
    );
    assert_ne!(
        UNSIGNED, SIGNED,
        "the two widenings must stay distinguishable for this test to mean anything"
    );
}

/// The compound arm. `dbChannel_put` maps `oldDBR_STS_CHAR` to `DBR_UCHAR`
/// exactly as it maps the bare type, so the metadata skip must not change the
/// carrier the value lands in.
#[test]
fn a_compound_char_write_widens_unsigned_on_both_loops() {
    let payload = {
        let mut p = vec![0u8; STS_CHAR_META];
        // Non-default status/severity, so a test failure that swapped the
        // metadata in for the value would be obvious.
        p[0..2].copy_from_slice(&3u16.to_be_bytes());
        p[2..4].copy_from_slice(&2u16.to_be_bytes());
        p.extend_from_slice(&WIRE_BYTES);
        p
    };
    both_loops_land(
        "X:WF",
        &|sid| write_frame(sid, DBR_STS_CHAR, 2, &payload),
        2,
        &UNSIGNED,
    );
}

// ---------------------------------------------------------------------------
// The get direction stays signed
// ---------------------------------------------------------------------------

/// C is asymmetric on purpose: the read keeps `oldDBR_CHAR` → `DBR_CHAR`
/// (`db_access.c:816`), so a `DBF_CHAR` field holding 0xC8 reads back as −56
/// through `getCharLong`. The bytes that went in are unchanged; only the
/// widening the reader asks for is signed. Pins the half that must NOT move.
#[test]
fn a_dbf_char_field_still_reads_back_signed_on_both_loops() {
    both_loops_land(
        "X:BYTES",
        &|sid| write_frame(sid, DBR_CHAR, 2, &WIRE_BYTES),
        2,
        &SIGNED,
    );
}
