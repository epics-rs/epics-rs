//! End-to-end proof that [`BlockingCaServer`] serves a **real CA client over
//! real sockets** with no async client anywhere — the coverage gap left when
//! `blocking_rtems_e2e` had to be gated off under `rtems-exec-model` (its
//! client side is the async `CaClient`, which needs a tokio reactor the
//! executor backend does not start).
//!
//! The client here is hand-rolled wire frames on a `std::net::TcpStream` /
//! `UdpSocket`. That is exactly the shape an RTEMS deployment faces: the
//! blocking front-end talking CA to a peer that this process does not drive
//! with tokio.
//!
//! **Feature-neutral by design.** Every test is a plain `#[test]` with no
//! runtime of any kind, and `BlockingCaServer` is compiled in both feature
//! states, so this file runs identically with `rtems-exec-model` on and off.
//! It therefore raises BOTH suite counts rather than only the feature-ON one:
//! the feature-OFF suite gains the same tests. That is deliberate — it means
//! the blocking front-end is guarded on the hosted default too, and any
//! divergence between the two execution backends shows up as this file
//! passing in one state and failing in the other.
//!
//! Coverage: VERSION handshake, CLIENT_NAME / HOST_NAME, CREATE_CHAN,
//! READ_NOTIFY, fire-and-forget WRITE, WRITE_NOTIFY (put-callback),
//! EVENT_ADD with a delivered monitor update, EVENT_CANCEL, CLEAR_CHANNEL,
//! and the UDP name-search responder. The full local command set landed in
//! S1c-a/S1c-b, so none of these may answer `ECA_UNAVAILINSERV`; every read
//! helper below fails the test on a `CA_PROTO_ERROR` frame, which is what
//! makes that a checked property rather than an assumption.
//!
//! Waits go through [`Circuit`], which never discards a frame — see the
//! `common/raw_ca.rs` module docs for why that matters when one write produces
//! two replies whose order the server does not fix.
//!
//! Ports are always ephemeral (`:0`) — never the real 5064, per the
//! `build() ⟹ listening` port-ownership rule.

#[path = "common/raw_ca.rs"]
mod raw_ca;

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::protocol::{
    CA_MINOR_VERSION, CA_PROTO_CLEAR_CHANNEL, CA_PROTO_EVENT_ADD, CA_PROTO_READ_NOTIFY,
    CA_PROTO_SEARCH, CA_PROTO_VERSION, CA_PROTO_WRITE, CA_PROTO_WRITE_NOTIFY, CaHeader, pad_string,
};
use epics_ca_rs::server::blocking::{BlockingCaServer, bind_udp_search};
use raw_ca::*;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A `PvDatabase` seeded on the calling thread. `block_on_sync` with no async
/// runtime entered selects `park_on`, so this works on a bare test thread
/// under either backend.
fn seed_db(pvs: &[(&str, EpicsValue)]) -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    for (name, value) in pvs {
        block_on_sync(db.add_pv(name, value.clone()))
            .expect("no async runtime on this test thread")
            .expect("add_pv");
    }
    db
}

/// The ioid a reply echoes, at bytes 12..16.
fn ioid_of(frame: &[u8]) -> u32 {
    u32::from_be_bytes([frame[12], frame[13], frame[14], frame[15]])
}

#[test]
fn blocking_server_serves_a_raw_socket_client_end_to_end() {
    let db = seed_db(&[("RAW:E2E", EpicsValue::Double(1.5))]);
    let (server, addr, accept) = start_server(db);

    let mut c = connect_and_handshake(addr);
    let sid = create_channel(&mut c, 0x1234, "RAW:E2E");

    // READ_NOTIFY sees the seeded value.
    c.send(&read_notify_frame(sid, 0x01));
    let r = c.expect(CA_PROTO_READ_NOTIFY, "READ_NOTIFY");
    assert_eq!(ioid_of(&r), 0x01, "READ_NOTIFY reply echoes our ioid");
    assert_eq!(
        payload_double(&r),
        1.5,
        "READ_NOTIFY returns the seeded VAL"
    );

    // Fire-and-forget WRITE: no reply frame, but the value must land.
    c.send(&write_frame(CA_PROTO_WRITE, sid, 0, 4.25));
    c.expect_silence(
        Duration::from_millis(250),
        "fire-and-forget WRITE must draw no reply",
    );
    c.send(&read_notify_frame(sid, 0x02));
    let r = c.expect(CA_PROTO_READ_NOTIFY, "READ_NOTIFY after WRITE");
    assert_eq!(
        payload_double(&r),
        4.25,
        "fire-and-forget WRITE took effect"
    );

    // WRITE_NOTIFY (put-callback): a synchronous record replies immediately.
    c.send(&write_frame(CA_PROTO_WRITE_NOTIFY, sid, 0x03, 7.5));
    let wn = c.expect(CA_PROTO_WRITE_NOTIFY, "WRITE_NOTIFY");
    assert_eq!(ioid_of(&wn), 0x03, "WRITE_NOTIFY reply echoes our ioid");
    c.send(&read_notify_frame(sid, 0x04));
    let r = c.expect(CA_PROTO_READ_NOTIFY, "READ_NOTIFY after WRITE_NOTIFY");
    assert_eq!(payload_double(&r), 7.5, "WRITE_NOTIFY took effect");

    // EVENT_ADD: initial snapshot, then a real update delivered over the
    // socket by the server's event path when the value changes.
    let sub_id = 0xAB;
    c.send(&event_add_frame(sid, sub_id));
    let initial = c.expect(CA_PROTO_EVENT_ADD, "EVENT_ADD initial");
    assert_eq!(
        ioid_of(&initial),
        sub_id,
        "monitor frame carries our subscription id"
    );
    assert_eq!(
        payload_double(&initial),
        7.5,
        "initial monitor update is the current VAL"
    );

    // One write, two replies: the put-callback acknowledgement and the monitor
    // update it fans out. The server fixes no order between them, so claim both
    // rather than waiting on one and destroying the other.
    c.send(&write_frame(CA_PROTO_WRITE_NOTIFY, sid, 0x05, 99.0));
    let pair = c.expect_all(
        &[CA_PROTO_WRITE_NOTIFY, CA_PROTO_EVENT_ADD],
        "WRITE_NOTIFY and the monitor update it fans out",
    );
    assert_eq!(
        ioid_of(&pair[0]),
        0x05,
        "the fanning-out write is acknowledged, echoing our ioid"
    );
    assert_eq!(
        payload_double(&pair[1]),
        99.0,
        "a write fans out to the subscription as a monitor update"
    );

    // EVENT_CANCEL: the subscription stops delivering.
    c.send(&event_cancel_frame(sid, sub_id));
    let _ = c.expect_cancel_ack(sub_id);
    c.send(&write_frame(CA_PROTO_WRITE, sid, 0, 123.0));
    c.expect_silence(
        Duration::from_millis(250),
        "a cancelled subscription must deliver nothing",
    );

    // CLEAR_CHANNEL closes the channel; the circuit itself stays up.
    c.send(&clear_channel_frame(sid, 0x1234));
    let cl = c.expect(CA_PROTO_CLEAR_CHANNEL, "CLEAR_CHANNEL");
    assert_eq!(
        u32::from_be_bytes([cl[8], cl[9], cl[10], cl[11]]),
        sid,
        "CLEAR_CHANNEL reply echoes the sid"
    );

    drop(c);
    server.shutdown();
    accept.join().unwrap();
}

/// The wait helper must not depend on which of two racing replies lands first.
///
/// Against a real server that order is not fixed: measured over 25 rounds, the
/// `SimplePv` path sent the WRITE_NOTIFY acknowledgement first 25/25 times,
/// while the real-record path sent it first only 10/25 times and the monitor
/// update first the other 15. A test written against whichever order happens to
/// occur is therefore a test that passes by luck.
///
/// A fake peer here sends the pair in the *opposite* order to the one the e2e
/// above waits in — precisely the case a discarding reader gets wrong. Such a
/// reader consumes and throws away the monitor update while looking for the
/// acknowledgement, and then blocks forever on an update that no longer exists.
#[test]
fn a_racing_reply_pair_is_delivered_whichever_order_it_arrives_in() {
    /// A zero-payload reply, the shape a WRITE_NOTIFY acknowledgement takes.
    fn ack_frame(cmmd: u16, sid: u32, ioid: u32) -> Vec<u8> {
        let mut h = CaHeader::new(cmmd);
        h.data_type = DBR_DOUBLE;
        h.count = 1;
        h.cid = sid;
        h.available = ioid;
        h.to_bytes().to_vec()
    }

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral loopback port");
    let addr = listener.local_addr().unwrap();
    let peer = thread::spawn(move || {
        let (mut s, _) = listener.accept().expect("accept");
        // Monitor update FIRST, acknowledgement second — the order the tests
        // above do not wait in.
        s.write_all(&write_frame(CA_PROTO_EVENT_ADD, 1, 0xAB, 99.0))
            .unwrap();
        s.write_all(&ack_frame(CA_PROTO_WRITE_NOTIFY, 1, 0x05))
            .unwrap();
        // Hold the connection open until the client hangs up, so the reads
        // above cannot succeed merely because the socket closed.
        let mut sink = [0u8; 1];
        let _ = s.read(&mut sink);
    });

    let mut c = Circuit::new(TcpStream::connect(addr).expect("connect to fake peer"));
    let wn = c.expect(CA_PROTO_WRITE_NOTIFY, "acknowledgement sent second");
    assert_eq!(ioid_of(&wn), 0x05, "the acknowledgement echoes our ioid");
    let update = c.expect(CA_PROTO_EVENT_ADD, "monitor update sent first");
    assert_eq!(
        payload_double(&update),
        99.0,
        "the update survived a wait that was looking for the acknowledgement"
    );
    c.expect_silence(
        Duration::from_millis(100),
        "both frames were claimed, so nothing is left queued",
    );

    drop(c);
    peer.join().unwrap();
}

/// A second circuit opened after the first closed proves the accept loop
/// keeps serving — the per-client thread teardown does not wedge the server.
#[test]
fn blocking_server_accepts_a_second_circuit_after_the_first_closes() {
    let db = seed_db(&[("RAW:SEQ", EpicsValue::Double(10.0))]);
    let (server, addr, accept) = start_server(db);

    {
        let mut c = connect_and_handshake(addr);
        let sid = create_channel(&mut c, 1, "RAW:SEQ");
        c.send(&write_frame(CA_PROTO_WRITE_NOTIFY, sid, 1, 20.0));
        let _ = c.expect(CA_PROTO_WRITE_NOTIFY, "first circuit WRITE_NOTIFY");
    }

    let mut c2 = connect_and_handshake(addr);
    let sid2 = create_channel(&mut c2, 2, "RAW:SEQ");
    c2.send(&read_notify_frame(sid2, 1));
    let r = c2.expect(CA_PROTO_READ_NOTIFY, "second circuit READ_NOTIFY");
    assert_eq!(
        payload_double(&r),
        20.0,
        "the second circuit observes the first circuit's write"
    );

    drop(c2);
    server.shutdown();
    accept.join().unwrap();
}

/// The UDP name-search responder over a real datagram socket: a
/// VERSION+SEARCH datagram for a seeded PV draws a reply advertising the
/// server's TCP port, and an unknown PV draws nothing.
#[test]
fn blocking_server_answers_a_raw_udp_search() {
    let db = seed_db(&[("RAW:UDP", EpicsValue::Double(1.0))]);
    let server = Arc::new(
        BlockingCaServer::bind(
            "127.0.0.1:0",
            db,
            epics_base_rs::server::access_security::new_acf_cell(None),
        )
        .expect("bind ephemeral loopback port"),
    );
    let tcp_port = server.tcp_port();

    // Ephemeral responder port — never the real 5064.
    let resp = bind_udp_search(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).expect("bind responder");
    let resp_addr = resp.local_addr().unwrap();
    let srv = server.clone();
    let udp_thread = thread::spawn(move || srv.serve_udp_search(resp));

    let client = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    // VERSION prelude + SEARCH, the shape a real CA client broadcasts.
    let mut dg = {
        let mut h = CaHeader::new(CA_PROTO_VERSION);
        h.count = CA_MINOR_VERSION;
        h.data_type = 1; // sequenceNoIsValid
        h.cid = 0xABCD;
        h.to_bytes().to_vec()
    };
    dg.extend_from_slice(&{
        let padded = pad_string("RAW:UDP");
        let mut h = CaHeader::new(CA_PROTO_SEARCH);
        h.data_type = 5; // DO_REPLY
        h.cid = 0x42;
        h.available = 0x42;
        h.set_payload_size(padded.len(), CA_MINOR_VERSION as u32, CA_MINOR_VERSION)
            .expect("modern peer");
        let mut f = h.to_bytes().to_vec();
        f.extend_from_slice(&padded);
        f
    });
    client.send_to(&dg, resp_addr).unwrap();

    let mut rbuf = vec![0u8; 64 * 1024];
    let (n, from) = client.recv_from(&mut rbuf).expect("SEARCH reply");
    assert_eq!(from, resp_addr, "reply comes from the responder socket");
    let reply = &rbuf[..n];

    assert_eq!(
        cmmd_of(reply),
        CA_PROTO_VERSION,
        "reply leads with a VERSION echo"
    );
    let s = &reply[CaHeader::SIZE..];
    assert_eq!(
        cmmd_of(s),
        CA_PROTO_SEARCH,
        "second message is the SEARCH reply"
    );
    assert_eq!(
        u16::from_be_bytes([s[4], s[5]]),
        tcp_port,
        "SEARCH reply advertises the server's real TCP port"
    );
    assert_eq!(
        u32::from_be_bytes([s[12], s[13], s[14], s[15]]),
        0x42,
        "SEARCH reply echoes our cid"
    );

    // An unknown PV draws no reply at all (C has no UDP NOT_FOUND branch).
    let mut miss = {
        let mut h = CaHeader::new(CA_PROTO_VERSION);
        h.count = CA_MINOR_VERSION;
        h.data_type = 1;
        h.cid = 1;
        h.to_bytes().to_vec()
    };
    miss.extend_from_slice(&{
        let padded = pad_string("RAW:NOPE");
        let mut h = CaHeader::new(CA_PROTO_SEARCH);
        h.data_type = 5;
        h.cid = 0x43;
        h.available = 0x43;
        h.set_payload_size(padded.len(), CA_MINOR_VERSION as u32, CA_MINOR_VERSION)
            .expect("modern peer");
        let mut f = h.to_bytes().to_vec();
        f.extend_from_slice(&padded);
        f
    });
    client.send_to(&miss, resp_addr).unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(400)))
        .unwrap();
    let mut buf = [0u8; 1024];
    assert!(
        client.recv_from(&mut buf).is_err(),
        "an unknown PV must draw no UDP reply"
    );

    server.shutdown();
    udp_thread.join().unwrap().expect("responder exits cleanly");
}

/// The circuit that the UDP search advertises is real: search for the port
/// over UDP, then open a TCP circuit to exactly that port and read the PV —
/// the full client sequence a CA client performs, with no async anywhere.
#[test]
fn a_udp_search_leads_to_a_working_tcp_circuit() {
    let db = seed_db(&[("RAW:BOTH", EpicsValue::Double(3.25))]);
    let server = Arc::new(
        BlockingCaServer::bind(
            "127.0.0.1:0",
            db,
            epics_base_rs::server::access_security::new_acf_cell(None),
        )
        .expect("bind ephemeral loopback port"),
    );
    let resp = bind_udp_search(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).expect("bind responder");
    let resp_addr = resp.local_addr().unwrap();

    let srv_udp = server.clone();
    let udp_thread = thread::spawn(move || srv_udp.serve_udp_search(resp));
    let srv_tcp = server.clone();
    let accept = thread::spawn(move || srv_tcp.serve());

    let client = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut dg = {
        let mut h = CaHeader::new(CA_PROTO_VERSION);
        h.count = CA_MINOR_VERSION;
        h.data_type = 1;
        h.cid = 7;
        h.to_bytes().to_vec()
    };
    dg.extend_from_slice(&{
        let padded = pad_string("RAW:BOTH");
        let mut h = CaHeader::new(CA_PROTO_SEARCH);
        h.data_type = 5;
        h.cid = 0x99;
        h.available = 0x99;
        h.set_payload_size(padded.len(), CA_MINOR_VERSION as u32, CA_MINOR_VERSION)
            .expect("modern peer");
        let mut f = h.to_bytes().to_vec();
        f.extend_from_slice(&padded);
        f
    });
    client.send_to(&dg, resp_addr).unwrap();

    let mut rbuf = vec![0u8; 64 * 1024];
    let (n, _) = client.recv_from(&mut rbuf).expect("SEARCH reply");
    let s = &rbuf[CaHeader::SIZE..n];
    let advertised = u16::from_be_bytes([s[4], s[5]]);

    // Dial exactly the advertised port — not `local_addr()`.
    let mut c = connect_and_handshake(SocketAddr::from((Ipv4Addr::LOCALHOST, advertised)));
    let sid = create_channel(&mut c, 0x99, "RAW:BOTH");
    c.send(&read_notify_frame(sid, 1));
    let r = c.expect(CA_PROTO_READ_NOTIFY, "READ_NOTIFY on searched port");
    assert_eq!(
        payload_double(&r),
        3.25,
        "the port the UDP search advertised serves the PV"
    );

    drop(c);
    server.shutdown();
    accept.join().unwrap();
    udp_thread.join().unwrap().expect("responder exits cleanly");
}
