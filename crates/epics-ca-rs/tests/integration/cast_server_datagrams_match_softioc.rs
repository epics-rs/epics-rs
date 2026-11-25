//! The UDP name-search responder against `cast_server`/`camessage`, pinned
//! from softIoc R7.0.10.1 (8f5015b66) answering the same raw datagrams.

use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use epics_ca_rs::protocol::{CA_PROTO_SEARCH, CA_PROTO_VERSION};
use serial_test::serial;

fn hdr(cmmd: u16, size: u16, dt: u16, count: u16, p1: u32, p2: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    b.extend_from_slice(&cmmd.to_be_bytes());
    b.extend_from_slice(&size.to_be_bytes());
    b.extend_from_slice(&dt.to_be_bytes());
    b.extend_from_slice(&count.to_be_bytes());
    b.extend_from_slice(&p1.to_be_bytes());
    b.extend_from_slice(&p2.to_be_bytes());
    b
}

fn version() -> Vec<u8> {
    hdr(CA_PROTO_VERSION, 0, 0, 13, 0, 0)
}

/// A SEARCH for `LO`; the reply echoes `cid` in `m_available`.
fn search(cid: u32) -> Vec<u8> {
    let mut f = hdr(CA_PROTO_SEARCH, 8, 10, 13, cid, cid);
    f.extend_from_slice(b"LO\0\0\0\0\0\0");
    f
}

/// An extended header (`m_postsize == 0xffff`) whose annex carries `size`
/// and `count`; `raw` is the 16-bit `m_count` the annex replaces.
fn ext_hdr(cmmd: u16, dt: u16, raw: u16, cid: u32, size: u32, count: u32) -> Vec<u8> {
    let mut b = hdr(cmmd, 0xFFFF, dt, raw, cid, cid);
    b.extend_from_slice(&size.to_be_bytes());
    b.extend_from_slice(&count.to_be_bytes());
    b
}

fn ext_version(count: u32, raw: u16) -> Vec<u8> {
    ext_hdr(CA_PROTO_VERSION, 0, raw, 0, 0, count)
}

fn ext_search(cid: u32, count: u32, raw: u16) -> Vec<u8> {
    let mut f = ext_hdr(CA_PROTO_SEARCH, 10, raw, cid, 8, count);
    f.extend_from_slice(b"LO\0\0\0\0\0\0");
    f
}

/// The `m_available` of every SEARCH reply in `dg`.
fn reply_cids(dg: &[u8]) -> Vec<u32> {
    let mut cids = Vec::new();
    let mut at = 0;
    while at + 16 <= dg.len() {
        let size = u16::from_be_bytes([dg[at + 2], dg[at + 3]]) as usize;
        if dg[at..at + 2] == CA_PROTO_SEARCH.to_be_bytes() {
            cids.push(u32::from_be_bytes([
                dg[at + 12],
                dg[at + 13],
                dg[at + 14],
                dg[at + 15],
            ]));
        }
        at += 16 + size;
    }
    cids
}

/// Every reply cid `sock` receives up to the reply carrying `cid`.
fn replies_until(sock: &UdpSocket, cid: u32) -> Vec<u32> {
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut seen = Vec::new();
    let mut buf = [0u8; 2048];
    while !seen.contains(&cid) {
        let n = sock.recv(&mut buf).expect("the barrier's SEARCH reply");
        seen.extend(reply_cids(&buf[..n]));
    }
    seen
}

/// Every errlog line `f` produces, in order.
fn errlog_lines(f: impl FnOnce()) -> Vec<String> {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    epics_base_rs::runtime::log::errlog_flush();
    let listener = epics_base_rs::runtime::log::errlog_add_listener(move |line| {
        sink.lock().unwrap().push(line.to_string());
    });
    f();
    epics_base_rs::runtime::log::errlog_flush();
    epics_base_rs::runtime::log::errlog_remove_listener(listener);
    seen.lock().unwrap().clone()
}

/// Sends each datagram followed by a barrier SEARCH that orders them;
/// returns the peer, the partial-message lines with the time stamp checked
/// and masked, and every reply cid.
fn exchange(udp: SocketAddr, datagrams: Vec<Vec<u8>>) -> (SocketAddr, Vec<String>, Vec<u32>) {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut cids = Vec::new();
    let lines = errlog_lines(|| {
        for (barrier, dg) in (0x100..).zip(datagrams) {
            sock.send_to(&dg, udp).unwrap();
            sock.send_to(&[version(), search(barrier)].concat(), udp)
                .unwrap();
            cids.extend(replies_until(&sock, barrier));
        }
    });
    let lines = lines
        .into_iter()
        .filter_map(|l| match l.strip_prefix("CAS: message received at ") {
            Some(at) => {
                chrono::NaiveDateTime::parse_from_str(at.trim_end(), "%Y-%m-%d %H:%M:%S")
                    .unwrap_or_else(|e| panic!("{l:?}: {e}"));
                Some("CAS: message received at <time>\n".to_string())
            }
            None => l.contains("partial (damaged?)").then_some(l),
        })
        .collect();
    (sock.local_addr().unwrap(), lines, cids)
}

/// A datagram under a header, a short tail, a body 8 bytes short and a
/// VERSION 8 bytes short.
fn partial_datagrams() -> Vec<Vec<u8>> {
    vec![
        vec![1u8; 8],
        vec![],
        [version(), search(7), vec![0; 4]].concat(),
        [
            version(),
            hdr(CA_PROTO_SEARCH, 16, 10, 13, 8, 8),
            b"LO\0\0\0\0\0\0".to_vec(),
        ]
        .concat(),
        [version(), hdr(CA_PROTO_VERSION, 8, 0, 13, 0, 0)].concat(),
    ]
}

fn softioc_partial_lines(peer: SocketAddr) -> Vec<String> {
    [8, 4, 24, 16]
        .iter()
        .flat_map(|n| {
            [
                format!("CAS: partial (damaged?) UDP msg of {n} bytes from {peer} ?\n"),
                "CAS: message received at <time>\n".to_string(),
            ]
        })
        .collect()
}

/// A SEARCH alone, a SEARCH before its VERSION, and a cut-short SEARCH
/// alone: softIoc answers none and prints nothing.
fn versionless_datagrams() -> Vec<Vec<u8>> {
    vec![
        search(0x21),
        [search(0x22), version()].concat(),
        [
            hdr(CA_PROTO_SEARCH, 16, 10, 13, 0x23, 0x23),
            b"LO\0\0\0\0\0\0".to_vec(),
        ]
        .concat(),
    ]
}

/// Behind a VERSION(13) every header may be extended, and its version is the
/// annex `m_count` (`camessage.c:2464-2471`), 32 bits wide (`server.h:100`):
/// softIoc answers 0x41, 0x43, 0x45, 0x46 and 0x47. An annex VERSION of 4
/// leaves the next header unextended, so its 0xffff postsize misaligns.
fn extended_minor_datagrams() -> Vec<Vec<u8>> {
    vec![
        [version(), ext_search(0x41, 13, 0)].concat(),
        [version(), ext_search(0x42, 3, 13)].concat(),
        [version(), ext_search(0x43, 0x10000, 0)].concat(),
        [version(), ext_version(3, 13), search(0x44)].concat(),
        [version(), ext_version(13, 0), search(0x45)].concat(),
        [version(), ext_version(0x10000, 0), search(0x46)].concat(),
        [version(), ext_version(0x10003, 13), search(0x47)].concat(),
        [version(), ext_version(4, 13), ext_search(0x48, 13, 13)].concat(),
    ]
}

const EXTENDED_MINOR_CIDS: [u32; 13] = [
    0x41, 0x100, 0x101, 0x43, 0x102, 0x103, 0x45, 0x104, 0x46, 0x105, 0x47, 0x106, 0x107,
];

/// The VERSION header leading the reply to `version` plus a SEARCH, if any.
fn reply_version(udp: SocketAddr, version: Vec<u8>) -> Option<Vec<u8>> {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    sock.send_to(&[version, search(0x60)].concat(), udp)
        .unwrap();
    let mut buf = [0u8; 2048];
    sock.recv(&mut buf).expect("the SEARCH reply");
    (buf[..2] == CA_PROTO_VERSION.to_be_bytes()).then(|| buf[..16].to_vec())
}

/// `udp_version_action` keeps a V4.11 VERSION's `m_cid` whatever its
/// `m_dataType` (`camessage.c:2157-2158`), and `cas_send_dg_msg` marks the
/// echo `sequenceNoIsValid` (`caserverio.c:194-196`), as softIoc answers.
fn version_echoes(udp: SocketAddr) -> Vec<Option<Vec<u8>>> {
    [(0, 13, 0), (0, 13, 7), (1, 13, 7), (5, 13, 7), (1, 10, 7)]
        .into_iter()
        .map(|(dt, minor, cid)| reply_version(udp, hdr(CA_PROTO_VERSION, 0, dt, minor, cid, 0)))
        .collect()
}

fn softioc_version_echoes() -> Vec<Option<Vec<u8>>> {
    let echo = |cid| Some(hdr(CA_PROTO_VERSION, 0, 1, 13, cid, 0));
    vec![echo(0), echo(7), echo(7), echo(7), None]
}

/// The datagram's last VERSION decides the reply header, an unsupported one
/// included: `udp_version_action` stores its minor before refusing it
/// (`camessage.c:2150-2153`).
fn last_version_echoes(udp: SocketAddr) -> Vec<Option<Vec<u8>>> {
    let v = |minor, cid| hdr(CA_PROTO_VERSION, 0, 0, minor, cid, 0);
    [
        [v(13, 7), search(0x70), v(10, 9)].concat(),
        [v(10, 9), search(0x72), v(13, 7)].concat(),
        [v(13, 7), search(0x74), v(2, 9)].concat(),
        [v(13, 7), search(0x76), v(12, 0)].concat(),
    ]
    .into_iter()
    .map(|prefix| reply_version(udp, prefix))
    .collect()
}

fn softioc_last_version_echoes() -> Vec<Option<Vec<u8>>> {
    let echo = |cid| Some(hdr(CA_PROTO_VERSION, 0, 1, 13, cid, 0));
    vec![None, echo(7), None, echo(0)]
}

/// `f` against the blocking front end's UDP responder.
fn with_blocking_responder<T>(f: impl FnOnce(SocketAddr) -> T) -> T {
    use epics_base_rs::types::EpicsValue;
    use epics_ca_rs::server::blocking::{BlockingCaServer, bind_udp_search};
    use std::sync::Arc;

    let db = Arc::new(epics_base_rs::server::database::PvDatabase::new());
    epics_base_rs::runtime::task::block_on_sync(db.add_pv("LO", EpicsValue::Long(0)))
        .expect("no runtime on this thread")
        .expect("add_pv");
    let server = Arc::new(
        BlockingCaServer::bind(
            "127.0.0.1:0",
            db,
            epics_base_rs::server::access_security::new_acf_cell(None),
        )
        .expect("bind"),
    );
    let resp = bind_udp_search(std::net::SocketAddrV4::new(
        std::net::Ipv4Addr::LOCALHOST,
        0,
    ))
    .expect("bind responder");
    let udp = resp.local_addr().unwrap();
    let srv = server.clone();
    let responder = std::thread::spawn(move || srv.serve_udp_search(resp));
    let got = f(udp);
    server.shutdown();
    responder.join().unwrap().expect("responder exits cleanly");
    got
}

/// `cast_server.c:228-240` after `camessage` stops short of the end.
#[test]
#[serial]
fn the_blocking_responder_reports_a_partial_datagram() {
    let (peer, lines, cids) = with_blocking_responder(|udp| exchange(udp, partial_datagrams()));
    assert_eq!(lines, softioc_partial_lines(peer));
    assert!(cids.contains(&7), "the SEARCH before the tail: {cids:x?}");
}

#[cfg(tokio_backend)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_async_responder_reports_a_partial_datagram() {
    let udp = async_responder().await;
    let (peer, lines, cids) =
        tokio::task::spawn_blocking(move || exchange(udp, partial_datagrams()))
            .await
            .unwrap();
    assert_eq!(lines, softioc_partial_lines(peer));
    assert!(cids.contains(&7), "the SEARCH before the tail: {cids:x?}");
}

/// `camessage.c:2432-2449`: a UDP client is unidentified until a supported
/// VERSION in the same datagram (`cast_server.c:197` resets it), and its
/// other messages end the datagram silently until then.
#[test]
#[serial]
fn the_blocking_responder_drops_a_datagram_led_by_no_version() {
    let (_, lines, cids) = with_blocking_responder(|udp| exchange(udp, versionless_datagrams()));
    assert_eq!(lines, Vec::<String>::new());
    assert_eq!(cids, [0x100, 0x101, 0x102]);
}

#[cfg(tokio_backend)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_async_responder_drops_a_datagram_led_by_no_version() {
    let udp = async_responder().await;
    let (_, lines, cids) =
        tokio::task::spawn_blocking(move || exchange(udp, versionless_datagrams()))
            .await
            .unwrap();
    assert_eq!(lines, Vec::<String>::new());
    assert_eq!(cids, [0x100, 0x101, 0x102]);
}

#[test]
#[serial]
fn the_blocking_responder_reads_the_minor_from_the_annex() {
    let (_, lines, cids) = with_blocking_responder(|udp| exchange(udp, extended_minor_datagrams()));
    assert_eq!(lines, Vec::<String>::new());
    assert_eq!(cids, EXTENDED_MINOR_CIDS);
}

#[cfg(tokio_backend)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_async_responder_reads_the_minor_from_the_annex() {
    let udp = async_responder().await;
    let (_, lines, cids) =
        tokio::task::spawn_blocking(move || exchange(udp, extended_minor_datagrams()))
            .await
            .unwrap();
    assert_eq!(lines, Vec::<String>::new());
    assert_eq!(cids, EXTENDED_MINOR_CIDS);
}

#[test]
#[serial]
fn the_blocking_responder_echoes_any_v411_version_s_sequence() {
    assert_eq!(
        with_blocking_responder(version_echoes),
        softioc_version_echoes()
    );
}

#[cfg(tokio_backend)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_async_responder_echoes_any_v411_version_s_sequence() {
    let udp = async_responder().await;
    let got = tokio::task::spawn_blocking(move || version_echoes(udp))
        .await
        .unwrap();
    assert_eq!(got, softioc_version_echoes());
}

#[test]
#[serial]
fn the_blocking_responder_echoes_the_last_version() {
    assert_eq!(
        with_blocking_responder(last_version_echoes),
        softioc_last_version_echoes()
    );
}

#[cfg(tokio_backend)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn the_async_responder_echoes_the_last_version() {
    let udp = async_responder().await;
    let got = tokio::task::spawn_blocking(move || last_version_echoes(udp))
        .await
        .unwrap();
    assert_eq!(got, softioc_last_version_echoes());
}

#[cfg(tokio_backend)]
async fn async_responder() -> SocketAddr {
    let server = epics_ca_rs::server::CaServer::builder()
        .port(0)
        .db_string("record(longout,\"LO\"){}\n", &Default::default())
        .expect("db")
        .build()
        .await
        .expect("build CA server");
    let udp = server.udp_port();
    tokio::spawn(async move { server.run().await });
    SocketAddr::from(([127, 0, 0, 1], udp))
}

/// `cast_server.c:197-214` adopts each datagram's source before `camessage`,
/// so a datagram drained behind another peer's burst is logged as its own.
#[cfg(tokio_backend)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_drained_datagram_is_logged_under_its_own_source() {
    let udp = async_responder().await;
    let (peer, lines) = tokio::task::spawn_blocking(move || {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        let lines = errlog_lines(|| {
            // `a`'s burst keeps the responder draining when `b`'s lands.
            for cid in 0..200 {
                a.send_to(&[version(), search(cid)].concat(), udp).unwrap();
            }
            let empty = hdr(CA_PROTO_SEARCH, 0, 5, 13, 0x77, 0x88);
            b.send_to(&[version(), empty, search(0x99)].concat(), udp)
                .unwrap();
            replies_until(&b, 0x99);
        });
        (b.local_addr().unwrap(), lines)
    })
    .await
    .unwrap();
    let empty: Vec<_> = lines
        .iter()
        .filter(|l| l.contains("empty PV name"))
        .collect();
    assert_eq!(
        empty,
        [&format!(
            "CAS: request from {peer} => empty PV name in UDP search request?\n"
        )]
    );
}
