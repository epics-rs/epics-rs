//! Shared raw-socket CA client for the blocking-front-end e2e suites.
//!
//! Hand-rolled CA wire frames over `std::net::TcpStream` — no `CaClient`, no
//! tokio runtime, no `.await`. Used by `blocking_raw_client_e2e.rs` (SimplePv
//! fixtures, feature-neutral) and `blocking_real_record_e2e.rs` (IocBuilder
//! records, feature-ON only), so the wire logic is written once.
//!
//! Every reader fails the test on a `CA_PROTO_ERROR` frame and names the ECA
//! status, which is what makes "no command falls back to ECA_UNAVAILINSERV" a
//! checked property of both suites rather than an assumption.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_ca_rs::protocol::{
    CA_MINOR_VERSION, CA_PROTO_CLEAR_CHANNEL, CA_PROTO_CLIENT_NAME, CA_PROTO_CREATE_CHAN,
    CA_PROTO_ERROR, CA_PROTO_EVENT_ADD, CA_PROTO_EVENT_CANCEL, CA_PROTO_HOST_NAME,
    CA_PROTO_READ_NOTIFY, CA_PROTO_VERSION, CaHeader, pad_string,
};
use epics_ca_rs::server::blocking::BlockingCaServer;

/// DBR_DOUBLE — the scalar type every frame below uses.
pub const DBR_DOUBLE: u16 = 6;

/// Bind the server on an ephemeral loopback port and start its accept loop on
/// a dedicated `std::thread`. Returns the server (for `shutdown`), its
/// address, and the accept-loop join handle.
pub fn start_server(
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
pub fn read_one_frame(sock: &mut TcpStream) -> Vec<u8> {
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

pub fn cmmd_of(frame: &[u8]) -> u16 {
    u16::from_be_bytes([frame[0], frame[1]])
}

/// Read frames until one carries `cmmd`.
///
/// A `CA_PROTO_ERROR` frame fails the test immediately, naming the ECA status
/// it carried — that is how "no command answers ECA_UNAVAILINSERV" is
/// *checked* here rather than assumed. The frame cap plus the socket read
/// timeout bound a hang.
pub fn read_until(sock: &mut TcpStream, cmmd: u16, what: &str) -> Vec<u8> {
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
pub fn read_cancel_ack(sock: &mut TcpStream, sub_id: u32) -> Vec<u8> {
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
pub fn expect_silence(sock: &mut TcpStream, dur: Duration, what: &str) {
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
pub fn payload_double(frame: &[u8]) -> f64 {
    f64::from_be_bytes([
        frame[16], frame[17], frame[18], frame[19], frame[20], frame[21], frame[22], frame[23],
    ])
}

pub fn version_frame() -> Vec<u8> {
    let mut h = CaHeader::new(CA_PROTO_VERSION);
    h.count = CA_MINOR_VERSION;
    h.to_bytes().to_vec()
}

pub fn string_payload_frame(cmmd: u16, s: &str) -> Vec<u8> {
    let padded = pad_string(s);
    let mut h = CaHeader::new(cmmd);
    h.set_payload_size(padded.len(), 0, CA_MINOR_VERSION)
        .expect("modern peer");
    let mut f = h.to_bytes().to_vec();
    f.extend_from_slice(&padded);
    f
}

pub fn create_chan_frame(cid: u32, pv: &str) -> Vec<u8> {
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

pub fn read_notify_frame(sid: u32, ioid: u32) -> Vec<u8> {
    let mut h = CaHeader::new(CA_PROTO_READ_NOTIFY);
    h.data_type = DBR_DOUBLE;
    h.count = 1;
    h.cid = sid;
    h.available = ioid;
    h.to_bytes().to_vec()
}

pub fn write_frame(cmmd: u16, sid: u32, ioid: u32, value: f64) -> Vec<u8> {
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

pub fn event_add_frame(sid: u32, sub_id: u32) -> Vec<u8> {
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

pub fn event_cancel_frame(sid: u32, sub_id: u32) -> Vec<u8> {
    let mut h = CaHeader::new(CA_PROTO_EVENT_CANCEL);
    h.data_type = DBR_DOUBLE;
    h.count = 1;
    h.cid = sid;
    h.available = sub_id;
    h.to_bytes().to_vec()
}

pub fn clear_channel_frame(sid: u32, cid: u32) -> Vec<u8> {
    let mut h = CaHeader::new(CA_PROTO_CLEAR_CHANNEL);
    h.cid = sid;
    h.available = cid;
    h.to_bytes().to_vec()
}

/// Open a circuit and complete the CA handshake: our VERSION + CLIENT_NAME +
/// HOST_NAME out, the server's VERSION in.
pub fn connect_and_handshake(addr: SocketAddr) -> TcpStream {
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
pub fn create_channel(c: &mut TcpStream, cid: u32, pv: &str) -> u32 {
    c.write_all(&create_chan_frame(cid, pv)).unwrap();
    let cc = read_until(c, CA_PROTO_CREATE_CHAN, "CREATE_CHAN");
    assert_eq!(
        u32::from_be_bytes([cc[8], cc[9], cc[10], cc[11]]),
        cid,
        "CREATE_CHAN reply echoes our cid"
    );
    u32::from_be_bytes([cc[12], cc[13], cc[14], cc[15]])
}
