//! **A zero-element CA write is a legal frame, not a short one.**
//!
//! `caput -a PV 0` puts `CA_PROTO_WRITE`/`CA_PROTO_WRITE_NOTIFY` on the wire
//! with `m_count == 0` and `m_postsize == 0`. C sizes the frame with
//! `dbr_size_n(mp->m_dataType, mp->m_count)` (`camessage.c:768` for
//! `write_action`, `:1692` for `write_notify_action`) and that macro is
//!
//! ```text
//! #define dbr_size_n(TYPE,COUNT)\
//! ((unsigned)((COUNT)<0?dbr_size[TYPE]:dbr_size[TYPE]+((COUNT)-1)*dbr_value_size[TYPE]))
//! ```
//!
//! — `db_access.h:533-534`. The special arm is `COUNT < 0`, and `m_count`
//! is unsigned, so on the wire that arm is unreachable: count 0 sizes
//! `dbr_size[t] + (0-1)*dbr_value_size[t]`, which is 0 for every DBR code.
//! `0 > m_postsize` is false, C accepts, and the record lands `NORD 0`,
//! `UDF 0`, `STAT/SEVR NO_ALARM`.
//!
//! The port had read that arm as `COUNT <= 0` and clamped the count to one
//! element before sizing, so every zero-length array write was measured
//! against an 8-byte body, came back short, and a short frame is C's silent
//! `RSRV_ERROR` — which drops the circuit. `caput -a` reported
//! "Virtual circuit disconnect" and the record stayed `UDF 1`.
//!
//! Measured on the same `.db` against a real C `softIoc` and this port
//! before the fix:
//!
//! ```text
//! C     caput -a ZL:WF 0 → accepted; NORD 0 STAT NO_ALARM SEVR NO_ALARM UDF 0
//! port  caput -a ZL:WF 0 → CA.Client.Exception "Virtual circuit disconnect"
//!                          NORD 0 STAT UDF SEVR INVALID UDF 1
//! ```
//!
//! The boundary that must NOT move with the fix is the genuinely short
//! frame: `m_count` elements declared with fewer bytes behind them is still
//! `RSRV_ERROR`, still a dropped circuit. Both are pinned below, and every
//! case drives BOTH receive loops, because they share one write head
//! (`serve_write_head`) and a test that proved only one of them is what lets
//! a shared-gate defect survive a review round.
//!
//! Ports are always ephemeral (`:0`) — never the real 5064, per the
//! `build() ⟹ listening` port-ownership rule.

#![cfg(tokio_backend)]

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
    CA_PROTO_WRITE_NOTIFY, CaHeader, ECA_NORMAL,
};
use epics_ca_rs::server::CaServer;
use epics_ca_rs::server::blocking::BlockingCaServer;

const DB: &str = r#"
record(waveform,"X:WF") { field(FTVL,"DOUBLE") field(NELM,"16") }
"#;

const DBR_DOUBLE: u16 = 6;
const DBR_LONG: u16 = 5;

/// `INVALID_ALARM` — the severity a waveform carries while `UDF` is still 1.
const SEVR_INVALID: i32 = 3;

/// How long a read waits before the circuit is declared silent.
const READ_TIMEOUT: Duration = Duration::from_millis(2000);

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
// A raw client that reads whole frames and can survive a closed circuit
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
        // A server that has already dropped the circuit turns this into
        // EPIPE; that is the outcome under test, not a harness failure.
        let _ = self.sock.write_all(frame);
    }

    /// Read one whole frame, or `None` if the circuit closed or went silent.
    fn next_frame(&mut self) -> Option<Vec<u8>> {
        let mut hdr = [0u8; CaHeader::SIZE];
        self.sock.read_exact(&mut hdr).ok()?;
        let mut frame = hdr.to_vec();
        let mut body = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
        if body == 0xFFFF {
            let mut annex = [0u8; 8];
            self.sock.read_exact(&mut annex).ok()?;
            frame.extend_from_slice(&annex);
            body = u32::from_be_bytes([annex[0], annex[1], annex[2], annex[3]]) as usize;
        }
        if body > 0 {
            let mut rest = vec![0u8; body];
            self.sock.read_exact(&mut rest).ok()?;
            frame.extend_from_slice(&rest);
        }
        Some(frame)
    }

    /// Read past the frames this conversation does not care about
    /// (`CA_PROTO_ACCESS_RIGHTS`, the server's own `CA_PROTO_VERSION`).
    fn read_until(&mut self, cmmd: u16) -> Option<Vec<u8>> {
        for _ in 0..8 {
            let frame = self.next_frame()?;
            if u16::from_be_bytes([frame[0], frame[1]]) == cmmd {
                return Some(frame);
            }
        }
        None
    }

    fn open(&mut self, cid: u32, pv: &str) -> u32 {
        self.send(&create_chan_frame(cid, pv));
        let created = self
            .read_until(CA_PROTO_CREATE_CHAN)
            .unwrap_or_else(|| panic!("no CREATE_CHAN reply for {pv}"));
        u32::from_be_bytes([created[12], created[13], created[14], created[15]])
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

/// A write frame carrying exactly `payload` under `data_type`/`count`. The
/// caller decides both, so a case can declare a count its payload does not
/// cover — that is the short-frame boundary.
fn write_frame(
    cmmd: u16,
    sid: u32,
    ioid: u32,
    data_type: u16,
    count: u32,
    payload: &[u8],
) -> Vec<u8> {
    let padded = payload.len().div_ceil(8) * 8;
    let mut h = CaHeader::new(cmmd);
    h.data_type = data_type;
    h.cid = sid;
    h.available = ioid;
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
    h.cid = sid;
    h.available = ioid;
    h.set_payload_size(0, count, CA_MINOR_VERSION)
        .expect("modern peer");
    h.to_bytes().to_vec()
}

fn first_long(frame: &[u8]) -> i32 {
    let off = CaHeader::SIZE;
    i32::from_be_bytes([frame[off], frame[off + 1], frame[off + 2], frame[off + 3]])
}

// ---------------------------------------------------------------------------
// What one conversation observed
// ---------------------------------------------------------------------------

/// Everything a case needs to decide, gathered in one circuit so that a
/// dropped circuit is itself one of the observations.
#[derive(Debug, PartialEq, Eq)]
struct Observed {
    /// The circuit answered a `READ_NOTIFY` after the write.
    survived: bool,
    /// `WRITE_NOTIFY` status, `None` for the fire-and-forget `WRITE`.
    status: Option<u32>,
    /// `X:WF.NORD` / `X:WF.SEVR`, read only when the circuit survived.
    nord: Option<i32>,
    sevr: Option<i32>,
}

fn run_case(addr: SocketAddr, write: &dyn Fn(u32) -> Vec<u8>, is_notify: bool) -> Observed {
    let mut c = Raw::connect(addr);
    c.send(&version_frame());
    let sid = c.open(0x2A, "X:WF");

    c.send(&write(sid));
    let status = if is_notify {
        // `write_notify_reply` carries the ECA status in `m_cid` and echoes
        // the request's type/count (`camessage.c:1407-1410`).
        c.read_until(CA_PROTO_WRITE_NOTIFY)
            .map(|f| u32::from_be_bytes([f[8], f[9], f[10], f[11]]))
    } else {
        None
    };

    // The circuit is alive iff it still answers. A server that treated the
    // write as a protocol violation has already closed by now.
    c.send(&read_notify_frame(sid, 0x99, DBR_DOUBLE, 4));
    let survived = c.read_until(CA_PROTO_READ_NOTIFY).is_some();

    let (nord, sevr) = if survived {
        let nord_sid = c.open(0x2B, "X:WF.NORD");
        c.send(&read_notify_frame(nord_sid, 0x9A, DBR_LONG, 1));
        let nord = c.read_until(CA_PROTO_READ_NOTIFY).map(|f| first_long(&f));
        let sevr_sid = c.open(0x2C, "X:WF.SEVR");
        c.send(&read_notify_frame(sevr_sid, 0x9B, DBR_LONG, 1));
        let sevr = c.read_until(CA_PROTO_READ_NOTIFY).map(|f| first_long(&f));
        (nord, sevr)
    } else {
        (None, None)
    };

    Observed {
        survived,
        status,
        nord,
        sevr,
    }
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

fn against_blocking(write: &dyn Fn(u32) -> Vec<u8>, is_notify: bool) -> Observed {
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

    let got = run_case(addr, write, is_notify);

    server.shutdown();
    let _ = accept.join();
    got
}

fn against_async(write: &dyn Fn(u32) -> Vec<u8>, is_notify: bool) -> Observed {
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
    let got = run_case(addr, write, is_notify);

    let _ = stop_tx.send(());
    let _ = server_thread.join();
    got
}

/// Run one script against both loops and assert each observed `expect`.
fn both_loops_observe(write: &dyn Fn(u32) -> Vec<u8>, is_notify: bool, expect: &Observed) {
    let blocking = against_blocking(write, is_notify);
    let asynchronous = against_async(write, is_notify);
    // Name every loop that got it wrong, not just the first: a failure that
    // stopped at the blocking driver would leave the async driver's answer
    // unstated.
    let wrong: Vec<String> = [(Loops::Blocking, &blocking), (Loops::Async, &asynchronous)]
        .into_iter()
        .filter(|(_, got)| *got != expect)
        .map(|(which, got)| format!("{which} observed {got:?}"))
        .collect();
    assert!(
        wrong.is_empty(),
        "expected {expect:?} (C dbr_size_n over an unsigned m_count); {}",
        wrong.join("; ")
    );
    assert_eq!(
        blocking, asynchronous,
        "the two CA receive loops disagree — the write size gate is not shared"
    );
}

// ---------------------------------------------------------------------------
// The zero-length write: accepted, circuit intact, record not UDF
// ---------------------------------------------------------------------------

/// `caput -a X:WF 0` on the deprecated fire-and-forget opcode.
#[test]
fn a_zero_count_write_keeps_the_circuit_and_clears_udf() {
    both_loops_observe(
        &|sid| write_frame(CA_PROTO_WRITE, sid, 0, DBR_DOUBLE, 0, &[]),
        false,
        &Observed {
            survived: true,
            status: None,
            nord: Some(0),
            sevr: Some(0),
        },
    );
}

/// The same count on `CA_PROTO_WRITE_NOTIFY`, which must also answer
/// `ECA_NORMAL` rather than vanish with the circuit.
#[test]
fn a_zero_count_write_notify_replies_normal() {
    both_loops_observe(
        &|sid| write_frame(CA_PROTO_WRITE_NOTIFY, sid, 0x77, DBR_DOUBLE, 0, &[]),
        true,
        &Observed {
            survived: true,
            status: Some(ECA_NORMAL),
            nord: Some(0),
            sevr: Some(0),
        },
    );
}

/// `DBR_PUT_ACKT` travels the same wire opcode through its own size gate
/// (`camessage.c:1658` sizes every data type, 35/36 included), so the
/// zero-count arm has to agree there too.
#[test]
fn a_zero_count_alarm_ack_write_keeps_the_circuit() {
    both_loops_observe(
        &|sid| {
            write_frame(
                CA_PROTO_WRITE,
                sid,
                0,
                epics_base_rs::types::DBR_PUT_ACKT,
                0,
                &[],
            )
        },
        false,
        // An alarm acknowledge writes no value, so the waveform is still
        // untouched: measured on a C `softIoc` over this same `.db`, a
        // waveform that has never been put reads `NORD 0`, `STAT UDF`,
        // `SEVR INVALID`, `UDF 1`.
        &Observed {
            survived: true,
            status: None,
            nord: Some(0),
            sevr: Some(SEVR_INVALID),
        },
    );
}

// ---------------------------------------------------------------------------
// The boundary the fix must not move
// ---------------------------------------------------------------------------

/// Four declared `DBR_DOUBLE` elements behind 8 bytes is a genuinely short
/// frame: `dbr_size_n(DBR_DOUBLE,4) = 32 > m_postsize`, C's silent
/// `RSRV_ERROR`, circuit dropped. Zero-count acceptance must not have
/// widened into this.
#[test]
fn a_short_frame_still_drops_the_circuit() {
    both_loops_observe(
        &|sid| write_frame(CA_PROTO_WRITE, sid, 0, DBR_DOUBLE, 4, &[0u8; 8]),
        false,
        &Observed {
            survived: false,
            status: None,
            nord: None,
            sevr: None,
        },
    );
}

/// One element behind its full 8 bytes is the ordinary case, and it must
/// still land — the sizer is exercised at count 1 as well as count 0.
#[test]
fn a_single_element_write_still_lands() {
    both_loops_observe(
        &|sid| write_frame(CA_PROTO_WRITE, sid, 0, DBR_DOUBLE, 1, &2.5f64.to_be_bytes()),
        false,
        &Observed {
            survived: true,
            status: None,
            nord: Some(1),
            sevr: Some(0),
        },
    );
}
