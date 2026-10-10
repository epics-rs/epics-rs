//! A CA `DBR_STRING` write to a `DBF_ENUM` VAL goes through the record's
//! `put_enum_str` (`biRecord.c`, `boRecord.c`, `mbbiRecord.c`,
//! `mbboRecord.c`, busy's `busyRecord.c`), then `putStringEnum`'s numeric
//! fallback bounded by `get_enum_strs`' `no_str` (`dbConvert.c:1149-1190`),
//! and a refusal is reported with `recGblRecordError(S_db_badChoice, precord,
//! pfrom)`.
//!
//! Measured on oracleBusyIoc (base R7.0.10 + busy, `st-busy.cmd`) with the
//! fixture below, one raw `CA_PROTO_WRITE_NOTIFY` `DBR_STRING` per case, then
//! `caget VAL UDF`. Every refusal answered `ECA_PUTFAIL` and printed
//! `recGblRecordError: <string> Illegal choice PV: <record>`; `busy` put to 1
//! completes only when the record goes idle, so that notify stays pending.

#![cfg(tokio_backend)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush};
use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_ca_rs::protocol::{
    CA_MINOR_VERSION, CA_PROTO_CREATE_CHAN, CA_PROTO_READ_NOTIFY, CA_PROTO_VERSION,
    CA_PROTO_WRITE_NOTIFY, CaHeader,
};
use epics_ca_rs::server::blocking::BlockingCaServer;

const DB: &str = r#"
record(bi, "BI") { field(ZNAM, "Off") field(ONAM, "On") }
record(bo, "BO") { field(ZNAM, "Off") field(ONAM, "On") }
record(mbbi, "MI") { field(ZRST, "Zero") field(ONST, "One") field(TWST, "Two") }
record(mbbo, "MO") { field(ZRST, "Zero") field(ONST, "One") field(TWST, "Two") }
record(busy, "BU") { field(ZNAM, "Done") field(ONAM, "Busy") }
"#;

const DBR_STRING: u16 = 0;
const DBR_LONG: u16 = 5;
const ECA_NORMAL: u32 = 1;
const ECA_PUTFAIL: u32 = 0xa0;

/// What one put left behind: the notify status (`None` while pending), VAL,
/// UDF.
type Outcome = (Option<u32>, i32, i32);

struct Raw {
    sock: TcpStream,
}

impl Raw {
    fn next_frame(&mut self) -> Option<Vec<u8>> {
        let mut hdr = [0u8; CaHeader::SIZE];
        if let Err(e) = self.sock.read_exact(&mut hdr) {
            assert!(
                matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ),
                "{e}"
            );
            return None;
        }
        let body = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
        let mut frame = hdr.to_vec();
        let mut rest = vec![0u8; body];
        self.sock.read_exact(&mut rest).expect("frame body");
        frame.extend_from_slice(&rest);
        Some(frame)
    }

    fn read_until(&mut self, cmmd: u16) -> Option<Vec<u8>> {
        while let Some(frame) = self.next_frame() {
            if u16::from_be_bytes([frame[0], frame[1]]) == cmmd {
                return Some(frame);
            }
        }
        None
    }

    fn channel(addr: SocketAddr, pv: &str) -> (Self, u32) {
        let sock = TcpStream::connect(addr).expect("connect");
        sock.set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let mut c = Self { sock };
        let mut v = CaHeader::new(CA_PROTO_VERSION);
        v.count = CA_MINOR_VERSION;
        c.sock.write_all(&v.to_bytes()).unwrap();
        let name = epics_ca_rs::protocol::pad_string(pv);
        let mut h = CaHeader::new(CA_PROTO_CREATE_CHAN);
        h.postsize = name.len() as u16;
        h.cid = 1;
        h.available = CA_MINOR_VERSION as u32;
        let mut f = h.to_bytes().to_vec();
        f.extend_from_slice(&name);
        c.sock.write_all(&f).unwrap();
        let created = c.read_until(CA_PROTO_CREATE_CHAN).expect("channel");
        let sid = u32::from_be_bytes([created[12], created[13], created[14], created[15]]);
        (c, sid)
    }

    fn read_long(&mut self, sid: u32) -> i32 {
        let mut h = CaHeader::new(CA_PROTO_READ_NOTIFY);
        h.data_type = DBR_LONG;
        h.count = 1;
        h.cid = sid;
        h.available = 9;
        self.sock.write_all(&h.to_bytes()).unwrap();
        let r = self.read_until(CA_PROTO_READ_NOTIFY).expect("read reply");
        let o = CaHeader::SIZE;
        i32::from_be_bytes([r[o], r[o + 1], r[o + 2], r[o + 3]])
    }
}

/// `DBR_STRING` `WRITE_NOTIFY` of `value` to `rec`, then VAL and UDF.
fn put(addr: SocketAddr, rec: &str, value: &str) -> Outcome {
    let (mut c, sid) = Raw::channel(addr, rec);
    let mut payload = [0u8; 40];
    payload[..value.len()].copy_from_slice(value.as_bytes());
    let mut h = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
    h.data_type = DBR_STRING;
    h.count = 1;
    h.cid = sid;
    h.available = 7;
    h.postsize = payload.len() as u16;
    let mut f = h.to_bytes().to_vec();
    f.extend_from_slice(&payload);
    c.sock.write_all(&f).unwrap();
    let status = c
        .read_until(CA_PROTO_WRITE_NOTIFY)
        .map(|r| u32::from_be_bytes([r[8], r[9], r[10], r[11]]));
    let (mut v, val_sid) = Raw::channel(addr, rec);
    let val = v.read_long(val_sid);
    let (mut u, udf_sid) = Raw::channel(addr, &format!("{rec}.UDF"));
    (status, val, u.read_long(udf_sid))
}

/// Play `cases` against `rec` on a fresh IOC; give back each outcome and
/// every errlog line.
fn run(rec: &str, cases: &[&str]) -> (Vec<Outcome>, Vec<String>) {
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&heard);
    errlog_add_listener(move |m| sink.lock().unwrap().push(m.trim_end().to_string()));

    let (db, _) = block_on_sync(
        IocBuilder::new()
            .register_record_type("busy", || Box::new(BusyRecord::default()))
            .db_string(DB, &HashMap::new())
            .unwrap()
            .build(),
    )
    .unwrap()
    .unwrap();
    errlog_flush();
    heard.lock().unwrap().clear();
    let server = Arc::new(
        BlockingCaServer::bind(
            "127.0.0.1:0",
            db,
            epics_base_rs::server::access_security::new_acf_cell(None),
        )
        .unwrap(),
    );
    let addr = server.local_addr().unwrap();
    let srv = server.clone();
    let accept = thread::spawn(move || srv.serve());

    let outcomes = cases.iter().map(|v| put(addr, rec, v)).collect();

    server.shutdown();
    let _ = accept.join();
    errlog_flush();
    let lines = std::mem::take(&mut *heard.lock().unwrap());
    (outcomes, lines)
}

fn refused(rec: &str, from: &str) -> String {
    format!("recGblRecordError: {from} Illegal choice PV: {rec}")
}

fn binary(rec: &str, name: &str) {
    let (got, lines) = run(rec, &[name, "Bogus", "0", "1", "2", "99"]);
    let ok = Some(ECA_NORMAL);
    let fail = Some(ECA_PUTFAIL);
    assert_eq!(
        got,
        [
            (ok, 1, 0),
            (fail, 1, 0),
            (ok, 0, 0),
            (ok, 1, 0),
            (fail, 1, 0),
            (fail, 1, 0)
        ]
    );
    assert_eq!(
        lines,
        [refused(rec, "Bogus"), refused(rec, "2"), refused(rec, "99")]
    );
}

fn multibit(rec: &str) {
    let (got, lines) = run(rec, &["Two", "Bogus", "1", "3", "99"]);
    let ok = Some(ECA_NORMAL);
    let fail = Some(ECA_PUTFAIL);
    assert_eq!(
        got,
        [
            (ok, 2, 0),
            (fail, 2, 0),
            (ok, 1, 0),
            (fail, 1, 0),
            (fail, 1, 0)
        ]
    );
    assert_eq!(
        lines,
        [refused(rec, "Bogus"), refused(rec, "3"), refused(rec, "99")]
    );
}

#[test]
fn bi() {
    binary("BI", "On");
}

#[test]
fn bo() {
    binary("BO", "On");
}

#[test]
fn mbbi() {
    multibit("MI");
}

#[test]
fn mbbo() {
    multibit("MO");
}

#[test]
fn busy() {
    let (got, lines) = run("BU", &["Bogus", "2", "99", "0", "Busy"]);
    let fail = Some(ECA_PUTFAIL);
    assert_eq!(
        got,
        [
            (fail, 0, 1),
            (fail, 0, 1),
            (fail, 0, 1),
            (Some(ECA_NORMAL), 0, 0),
            (None, 1, 0)
        ]
    );
    assert_eq!(
        lines,
        [
            refused("BU", "Bogus"),
            refused("BU", "2"),
            refused("BU", "99")
        ]
    );
}
