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
//! Ports are always ephemeral (`:0`) — never the real 5064, per the
//! `build() ⟹ listening` port-ownership rule.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::protocol::{
    CA_MINOR_VERSION, CA_PROTO_CLEAR_CHANNEL, CA_PROTO_CLIENT_NAME, CA_PROTO_CREATE_CHAN,
    CA_PROTO_ERROR, CA_PROTO_EVENT_ADD, CA_PROTO_EVENT_CANCEL, CA_PROTO_HOST_NAME,
    CA_PROTO_READ_NOTIFY, CA_PROTO_SEARCH, CA_PROTO_VERSION, CA_PROTO_WRITE, CA_PROTO_WRITE_NOTIFY,
    CaHeader, pad_string,
};
use epics_ca_rs::server::blocking::{BlockingCaServer, bind_udp_search};

/// DBR_DOUBLE — the scalar type every frame below uses.
const DBR_DOUBLE: u16 = 6;

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

/// Bind the server on an ephemeral loopback port and start its accept loop on
/// a dedicated `std::thread`. Returns the server (for `shutdown`), its
/// address, and the accept-loop join handle.
fn start_server(
    db: Arc<PvDatabase>,
) -> (Arc<BlockingCaServer>, SocketAddr, thread::JoinHandle<()>) {
    let server = Arc::new(
        BlockingCaServer::bind("127.0.0.1:0", db, Arc::new(tokio::sync::RwLock::new(None)))
            .expect("bind ephemeral loopback port"),
    );
    let addr = server.local_addr().expect("local_addr");
    let srv = server.clone();
    let accept = thread::spawn(move || srv.serve());
    (server, addr, accept)
}

// ---------------------------------------------------------------------------
// Raw wire client
// ---------------------------------------------------------------------------

/// Read exactly one whole CA frame (header + declared payload), leaving the
/// socket on a frame boundary.
fn read_one_frame(sock: &mut TcpStream) -> Vec<u8> {
    let mut hdr = [0u8; CaHeader::SIZE];
    sock.read_exact(&mut hdr).expect("read frame header");
    let postsize = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
    let mut frame = hdr.to_vec();
    if postsize > 0 {
        let mut body = vec![0u8; postsize];
        sock.read_exact(&mut body).expect("read frame body");
        frame.extend_from_slice(&body);
    }
    frame
}

fn cmmd_of(frame: &[u8]) -> u16 {
    u16::from_be_bytes([frame[0], frame[1]])
}

/// Read frames until one carries `cmmd`.
///
/// A `CA_PROTO_ERROR` frame fails the test immediately, naming the ECA status
/// it carried — that is how "no command answers ECA_UNAVAILINSERV" is
/// *checked* here rather than assumed. The frame cap plus the socket read
/// timeout bound a hang.
fn read_until(sock: &mut TcpStream, cmmd: u16, what: &str) -> Vec<u8> {
    for _ in 0..64 {
        let frame = read_one_frame(sock);
        let got = cmmd_of(&frame);
        if got == CA_PROTO_ERROR {
            let status = u32::from_be_bytes([frame[12], frame[13], frame[14], frame[15]]);
            panic!("{what}: server answered CA_PROTO_ERROR (ECA status {status:#x})");
        }
        if got == cmmd {
            return frame;
        }
    }
    panic!("{what}: no cmmd={cmmd} within 64 frames");
}

/// Read the EVENT_CANCEL acknowledgement.
///
/// C `event_cancel_reply` (`camessage.c:2002-2014`) echoes the stored
/// EVENT_ADD request — same data_type / count / sid / sub-id — with a **zero
/// payload**, rather than echoing the EVENT_CANCEL opcode. The zero postsize
/// is what distinguishes the ack from a genuine monitor update still in
/// flight, so that is what this matches on.
fn read_cancel_ack(sock: &mut TcpStream, sub_id: u32) -> Vec<u8> {
    for _ in 0..64 {
        let frame = read_one_frame(sock);
        let got = cmmd_of(&frame);
        if got == CA_PROTO_ERROR {
            let status = u32::from_be_bytes([frame[12], frame[13], frame[14], frame[15]]);
            panic!("EVENT_CANCEL: server answered CA_PROTO_ERROR (ECA status {status:#x})");
        }
        let postsize = u16::from_be_bytes([frame[2], frame[3]]);
        if got == CA_PROTO_EVENT_ADD && postsize == 0 {
            assert_eq!(
                u32::from_be_bytes([frame[12], frame[13], frame[14], frame[15]]),
                sub_id,
                "cancel ack carries the cancelled subscription id"
            );
            return frame;
        }
    }
    panic!("EVENT_CANCEL: no zero-payload EVENT_ADD ack within 64 frames");
}

/// Assert that no frame arrives within `dur` — used to prove a
/// fire-and-forget WRITE stays silent and a cancelled subscription stops.
fn expect_silence(sock: &mut TcpStream, dur: Duration, what: &str) {
    sock.set_read_timeout(Some(dur)).unwrap();
    let mut hdr = [0u8; CaHeader::SIZE];
    match sock.read_exact(&mut hdr) {
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
        {
            // expected: nothing on the wire
        }
        Ok(()) => panic!("{what}: expected silence, got cmmd={}", cmmd_of(&hdr)),
        Err(e) => panic!("{what}: unexpected socket error {e}"),
    }
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
}

/// The DBR_DOUBLE scalar in a reply payload (payload starts after the header).
fn payload_double(frame: &[u8]) -> f64 {
    f64::from_be_bytes([
        frame[16], frame[17], frame[18], frame[19], frame[20], frame[21], frame[22], frame[23],
    ])
}

fn version_frame() -> Vec<u8> {
    let mut h = CaHeader::new(CA_PROTO_VERSION);
    h.count = CA_MINOR_VERSION;
    h.to_bytes().to_vec()
}

fn string_payload_frame(cmmd: u16, s: &str) -> Vec<u8> {
    let padded = pad_string(s);
    let mut h = CaHeader::new(cmmd);
    h.set_payload_size(padded.len(), 0, CA_MINOR_VERSION)
        .expect("modern peer");
    let mut f = h.to_bytes().to_vec();
    f.extend_from_slice(&padded);
    f
}

fn create_chan_frame(cid: u32, pv: &str) -> Vec<u8> {
    let padded = pad_string(pv);
    let mut h = CaHeader::new(CA_PROTO_CREATE_CHAN);
    h.cid = cid;
    h.available = CA_MINOR_VERSION as u32;
    h.set_payload_size(padded.len(), 0, CA_MINOR_VERSION)
        .expect("modern peer");
    let mut f = h.to_bytes().to_vec();
    f.extend_from_slice(&padded);
    f
}

fn read_notify_frame(sid: u32, ioid: u32) -> Vec<u8> {
    let mut h = CaHeader::new(CA_PROTO_READ_NOTIFY);
    h.data_type = DBR_DOUBLE;
    h.count = 1;
    h.cid = sid;
    h.available = ioid;
    h.to_bytes().to_vec()
}

fn write_frame(cmmd: u16, sid: u32, ioid: u32, value: f64) -> Vec<u8> {
    let mut h = CaHeader::new(cmmd);
    h.data_type = DBR_DOUBLE;
    h.count = 1;
    h.cid = sid;
    h.available = ioid;
    h.set_payload_size(8, 1, CA_MINOR_VERSION)
        .expect("modern peer");
    let mut f = h.to_bytes().to_vec();
    f.extend_from_slice(&value.to_be_bytes());
    f
}

fn event_add_frame(sid: u32, sub_id: u32) -> Vec<u8> {
    let mut h = CaHeader::new(CA_PROTO_EVENT_ADD);
    h.data_type = DBR_DOUBLE;
    h.count = 1;
    h.cid = sid;
    h.available = sub_id;
    h.set_payload_size(16, 1, CA_MINOR_VERSION)
        .expect("modern peer");
    let mut f = h.to_bytes().to_vec();
    f.extend_from_slice(&0f32.to_be_bytes()); // low
    f.extend_from_slice(&0f32.to_be_bytes()); // high
    f.extend_from_slice(&0f32.to_be_bytes()); // to
    f.extend_from_slice(&3u16.to_be_bytes()); // mask: DBE_VALUE|DBE_ALARM
    f.extend_from_slice(&0u16.to_be_bytes()); // pad
    f
}

fn event_cancel_frame(sid: u32, sub_id: u32) -> Vec<u8> {
    let mut h = CaHeader::new(CA_PROTO_EVENT_CANCEL);
    h.data_type = DBR_DOUBLE;
    h.count = 1;
    h.cid = sid;
    h.available = sub_id;
    h.to_bytes().to_vec()
}

fn clear_channel_frame(sid: u32, cid: u32) -> Vec<u8> {
    let mut h = CaHeader::new(CA_PROTO_CLEAR_CHANNEL);
    h.cid = sid;
    h.available = cid;
    h.to_bytes().to_vec()
}

/// Open a circuit and complete the CA handshake: our VERSION + CLIENT_NAME +
/// HOST_NAME out, the server's VERSION in.
fn connect_and_handshake(addr: SocketAddr) -> TcpStream {
    let mut c = TcpStream::connect(addr).expect("connect to blocking server");
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    c.write_all(&version_frame()).unwrap();
    c.write_all(&string_payload_frame(CA_PROTO_CLIENT_NAME, "raw-e2e"))
        .unwrap();
    c.write_all(&string_payload_frame(CA_PROTO_HOST_NAME, "localhost"))
        .unwrap();
    let v = read_until(&mut c, CA_PROTO_VERSION, "handshake");
    assert_eq!(
        u16::from_be_bytes([v[6], v[7]]),
        CA_MINOR_VERSION,
        "server advertises its CA minor version in the VERSION echo"
    );
    c
}

/// CREATE_CHAN for `pv`; returns the server-assigned sid.
fn create_channel(c: &mut TcpStream, cid: u32, pv: &str) -> u32 {
    c.write_all(&create_chan_frame(cid, pv)).unwrap();
    let cc = read_until(c, CA_PROTO_CREATE_CHAN, "CREATE_CHAN");
    assert_eq!(
        u32::from_be_bytes([cc[8], cc[9], cc[10], cc[11]]),
        cid,
        "CREATE_CHAN reply echoes our cid"
    );
    u32::from_be_bytes([cc[12], cc[13], cc[14], cc[15]])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The full local TCP command set against the blocking front-end, driven by a
/// hand-rolled client: handshake → CREATE_CHAN → READ_NOTIFY → WRITE →
/// WRITE_NOTIFY → EVENT_ADD (initial + update) → EVENT_CANCEL →
/// CLEAR_CHANNEL. No async client, no runtime.
#[test]
fn blocking_server_serves_a_raw_socket_client_end_to_end() {
    let db = seed_db(&[("RAW:E2E", EpicsValue::Double(1.5))]);
    let (server, addr, accept) = start_server(db);

    let mut c = connect_and_handshake(addr);
    let sid = create_channel(&mut c, 0x1234, "RAW:E2E");

    // READ_NOTIFY sees the seeded value.
    c.write_all(&read_notify_frame(sid, 0x01)).unwrap();
    let r = read_until(&mut c, CA_PROTO_READ_NOTIFY, "READ_NOTIFY");
    assert_eq!(
        u32::from_be_bytes([r[12], r[13], r[14], r[15]]),
        0x01,
        "READ_NOTIFY reply echoes our ioid"
    );
    assert_eq!(
        payload_double(&r),
        1.5,
        "READ_NOTIFY returns the seeded VAL"
    );

    // Fire-and-forget WRITE: no reply frame, but the value must land.
    c.write_all(&write_frame(CA_PROTO_WRITE, sid, 0, 4.25))
        .unwrap();
    expect_silence(
        &mut c,
        Duration::from_millis(250),
        "fire-and-forget WRITE must draw no reply",
    );
    c.write_all(&read_notify_frame(sid, 0x02)).unwrap();
    let r = read_until(&mut c, CA_PROTO_READ_NOTIFY, "READ_NOTIFY after WRITE");
    assert_eq!(
        payload_double(&r),
        4.25,
        "fire-and-forget WRITE took effect"
    );

    // WRITE_NOTIFY (put-callback): a synchronous record replies immediately.
    c.write_all(&write_frame(CA_PROTO_WRITE_NOTIFY, sid, 0x03, 7.5))
        .unwrap();
    let wn = read_until(&mut c, CA_PROTO_WRITE_NOTIFY, "WRITE_NOTIFY");
    assert_eq!(
        u32::from_be_bytes([wn[12], wn[13], wn[14], wn[15]]),
        0x03,
        "WRITE_NOTIFY reply echoes our ioid"
    );
    c.write_all(&read_notify_frame(sid, 0x04)).unwrap();
    let r = read_until(
        &mut c,
        CA_PROTO_READ_NOTIFY,
        "READ_NOTIFY after WRITE_NOTIFY",
    );
    assert_eq!(payload_double(&r), 7.5, "WRITE_NOTIFY took effect");

    // EVENT_ADD: initial snapshot, then a real update delivered over the
    // socket by the server's event path when the value changes.
    let sub_id = 0xAB;
    c.write_all(&event_add_frame(sid, sub_id)).unwrap();
    let initial = read_until(&mut c, CA_PROTO_EVENT_ADD, "EVENT_ADD initial");
    assert_eq!(
        u32::from_be_bytes([initial[12], initial[13], initial[14], initial[15]]),
        sub_id,
        "monitor frame carries our subscription id"
    );
    assert_eq!(
        payload_double(&initial),
        7.5,
        "initial monitor update is the current VAL"
    );

    c.write_all(&write_frame(CA_PROTO_WRITE_NOTIFY, sid, 0x05, 99.0))
        .unwrap();
    let update = read_until(&mut c, CA_PROTO_EVENT_ADD, "EVENT_ADD update");
    assert_eq!(
        payload_double(&update),
        99.0,
        "a write fans out to the subscription as a monitor update"
    );

    // EVENT_CANCEL: the subscription stops delivering.
    c.write_all(&event_cancel_frame(sid, sub_id)).unwrap();
    let _ = read_cancel_ack(&mut c, sub_id);
    c.write_all(&write_frame(CA_PROTO_WRITE, sid, 0, 123.0))
        .unwrap();
    expect_silence(
        &mut c,
        Duration::from_millis(250),
        "a cancelled subscription must deliver nothing",
    );

    // CLEAR_CHANNEL closes the channel; the circuit itself stays up.
    c.write_all(&clear_channel_frame(sid, 0x1234)).unwrap();
    let cl = read_until(&mut c, CA_PROTO_CLEAR_CHANNEL, "CLEAR_CHANNEL");
    assert_eq!(
        u32::from_be_bytes([cl[8], cl[9], cl[10], cl[11]]),
        sid,
        "CLEAR_CHANNEL reply echoes the sid"
    );

    drop(c);
    server.shutdown();
    accept.join().unwrap();
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
        c.write_all(&write_frame(CA_PROTO_WRITE_NOTIFY, sid, 1, 20.0))
            .unwrap();
        let _ = read_until(&mut c, CA_PROTO_WRITE_NOTIFY, "first circuit WRITE_NOTIFY");
    }

    let mut c2 = connect_and_handshake(addr);
    let sid2 = create_channel(&mut c2, 2, "RAW:SEQ");
    c2.write_all(&read_notify_frame(sid2, 1)).unwrap();
    let r = read_until(&mut c2, CA_PROTO_READ_NOTIFY, "second circuit READ_NOTIFY");
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
        BlockingCaServer::bind("127.0.0.1:0", db, Arc::new(tokio::sync::RwLock::new(None)))
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
        BlockingCaServer::bind("127.0.0.1:0", db, Arc::new(tokio::sync::RwLock::new(None)))
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
    c.write_all(&read_notify_frame(sid, 1)).unwrap();
    let r = read_until(&mut c, CA_PROTO_READ_NOTIFY, "READ_NOTIFY on searched port");
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
