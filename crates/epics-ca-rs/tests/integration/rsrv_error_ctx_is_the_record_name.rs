//! The text a channel-scoped `CA_PROTO_ERROR` carries, pinned from softIoc
//! (8f5015b66) answering the same raw frames on `record(longout,"LO"){}`:
//!
//! ```text
//! READ type=39 on LO.VAL       ECA_BADTYPE  ctx='LO'
//! EVENT_ADD mask=0 on LO.VAL   ECA_ALLOCMEM ctx='subscription install into record LO failed'
//! EVENT_ADD mask=0x100 on LO   ECA_ALLOCMEM ctx='subscription install into record LO failed'
//! ```
//!
//! None is reachable from caget/camonitor: libca never sends a bad READ type
//! and refuses a zero mask itself (`ECA_BADMASK`).

#![cfg(tokio_backend)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use epics_base_rs::server::records::longout::LongoutRecord;
use epics_ca_rs::protocol::{
    CA_MINOR_VERSION, CA_PROTO_CREATE_CHAN, CA_PROTO_ERROR, CA_PROTO_EVENT_ADD, CA_PROTO_READ,
    CA_PROTO_VERSION, CaHeader, ECA_ALLOCMEM, ECA_BADTYPE, pad_string,
};
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// Every frame the server sends until it closes the circuit.
fn frames_until_eof(sock: &mut TcpStream) -> Vec<(CaHeader, Vec<u8>)> {
    let mut acc = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match sock.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => acc.extend_from_slice(&buf[..n]),
            Err(e) => panic!("server kept the circuit open: {e}"),
        }
    }
    let mut out = Vec::new();
    let mut off = 0;
    while off < acc.len() {
        let (hdr, size) = CaHeader::from_bytes_extended(&acc[off..]).expect("whole header");
        let end = off + size + hdr.actual_postsize();
        out.push((hdr, acc[off + size..end].to_vec()));
        off = end;
    }
    out
}

/// Open `pv` and send `request` on it; return `(status, ctx)` of the error.
fn refuse(port: u16, pv: &str, request: impl Fn(u32) -> Vec<u8>) -> (u32, String) {
    let mut sock = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut version = CaHeader::new(CA_PROTO_VERSION);
    version.count = CA_MINOR_VERSION;
    let name = pad_string(pv);
    let mut create = CaHeader::new(CA_PROTO_CREATE_CHAN);
    create.cid = 1;
    create.available = CA_MINOR_VERSION as u32;
    create
        .set_payload_size(name.len(), 0, CA_MINOR_VERSION)
        .unwrap();
    let mut out = version.to_bytes().to_vec();
    out.extend_from_slice(&create.to_bytes());
    out.extend_from_slice(&name);
    sock.write_all(&out).unwrap();
    // The sid arrives before the circuit is torn down, so wait for it here.
    let mut acc = Vec::new();
    let sid = loop {
        let mut buf = [0u8; 256];
        let n = sock.read(&mut buf).expect("CREATE_CHAN reply");
        acc.extend_from_slice(&buf[..n]);
        let mut off = 0;
        let mut sid = None;
        while let Ok((hdr, size)) = CaHeader::from_bytes_extended(&acc[off..]) {
            let end = off + size + hdr.actual_postsize();
            if end > acc.len() {
                break;
            }
            if hdr.cmmd == CA_PROTO_CREATE_CHAN {
                sid = Some(hdr.available);
            }
            off = end;
        }
        if let Some(sid) = sid {
            break sid;
        }
    };
    sock.write_all(&request(sid)).unwrap();
    let (hdr, payload) = frames_until_eof(&mut sock)
        .into_iter()
        .find(|(h, _)| h.cmmd == CA_PROTO_ERROR)
        .expect("a CA_PROTO_ERROR before the close");
    let ctx = &payload[CaHeader::SIZE..];
    let ctx = &ctx[..ctx.iter().position(|&b| b == 0).unwrap_or(ctx.len())];
    (hdr.available, String::from_utf8_lossy(ctx).into_owned())
}

fn event_add(mask: u16) -> impl Fn(u32) -> Vec<u8> {
    move |sid| {
        let mut h = CaHeader::new(CA_PROTO_EVENT_ADD);
        h.data_type = 6;
        h.count = 1;
        h.cid = sid;
        h.available = 9;
        h.set_payload_size(16, 1, CA_MINOR_VERSION).unwrap();
        let mut f = h.to_bytes().to_vec();
        f.extend_from_slice(&[0u8; 12]);
        f.extend_from_slice(&mask.to_be_bytes());
        f.extend_from_slice(&[0u8; 2]);
        f
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_refused_request_carries_what_softioc_sends() {
    let server = CaServer::builder()
        .port(0)
        .record("LO", LongoutRecord::new(0))
        .build()
        .await
        .expect("build CA server");
    let port = server.tcp_port();
    tokio::spawn(async move { server.run().await });

    let got = tokio::task::spawn_blocking(move || {
        [
            refuse(port, "LO.VAL", |sid| {
                let mut h = CaHeader::new(CA_PROTO_READ);
                h.data_type = 39;
                h.count = 1;
                h.cid = sid;
                h.available = 7;
                h.to_bytes().to_vec()
            }),
            refuse(port, "LO.VAL", event_add(0)),
            refuse(port, "LO", event_add(0x100)),
        ]
    })
    .await
    .unwrap();
    let install = "subscription install into record LO failed".to_string();
    assert_eq!(
        got,
        [
            (ECA_BADTYPE, "LO".to_string()),
            (ECA_ALLOCMEM, install.clone()),
            (ECA_ALLOCMEM, install),
        ]
    );
}
