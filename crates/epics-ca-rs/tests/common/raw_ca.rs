//! Shared raw-socket CA client for the blocking-front-end e2e suites.
//!
//! Hand-rolled CA wire frames over `std::net::TcpStream` — no `CaClient`, no
//! tokio runtime, no `.await`. Used by `blocking_raw_client_e2e.rs` (SimplePv
//! fixtures, feature-neutral) and `blocking_real_record_e2e.rs` (IocBuilder
//! records, feature-ON only), so the wire logic is written once.
//!
//! # Frames are never discarded
//!
//! The obvious shape for a helper here — "read frames until one matches, drop
//! the rest" — is wrong, and was the source of a real defect. A single write
//! can produce two replies: the WRITE_NOTIFY acknowledgement and the monitor
//! update the write fans out. The server does not promise an order between
//! them, and measurably does not have one: over 25 rounds, the `SimplePv` path
//! sent WRITE_NOTIFY first 25/25 times, while the real-record path sent
//! WRITE_NOTIFY first only 10/25 times and the monitor update first the other
//! 15. A discarding reader therefore passes or fails by luck — it consumes and
//! throws away whichever reply it was not waiting for, so a later expectation
//! on that frame blocks forever, or a later silence check trips over it.
//!
//! [`Circuit`] closes that off by construction rather than by care at each call
//! site: it owns the socket *and* a queue of frames that have arrived but not
//! yet been matched. Every read consults the queue first and stashes what it
//! does not want, so no frame is ever lost, and the ordering the server happens
//! to pick stops being load-bearing. There is deliberately no discarding
//! primitive left in this module for a caller to reach for.
//!
//! One consequence is worth stating, because it is a feature: since nothing is
//! dropped, [`Circuit::expect_silence`] fails on a frame that arrived earlier
//! and was never claimed. A test must account for every frame the server sent
//! it, not merely for the ones it thought to look at.
//!
//! Every reader fails the test on a `CA_PROTO_ERROR` frame and names the ECA
//! status, which is what makes "no command falls back to ECA_UNAVAILINSERV" a
//! checked property of both suites rather than an assumption.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_ca_rs::protocol::{
    CA_MINOR_VERSION, CA_PROTO_ACCESS_RIGHTS, CA_PROTO_CLEAR_CHANNEL, CA_PROTO_CLIENT_NAME,
    CA_PROTO_CREATE_CHAN, CA_PROTO_ERROR, CA_PROTO_EVENT_ADD, CA_PROTO_EVENT_CANCEL,
    CA_PROTO_HOST_NAME, CA_PROTO_READ_NOTIFY, CA_PROTO_VERSION, CaHeader, pad_string,
};
use epics_ca_rs::server::blocking::BlockingCaServer;

/// DBR_DOUBLE — the scalar type every frame below uses.
pub const DBR_DOUBLE: u16 = 6;

/// How long a read waits before the test is declared hung.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Frames one expectation will look at before giving up. Bounds a hang in
/// company with the socket read timeout.
const FRAME_BUDGET: usize = 64;

/// Bind the server on an ephemeral loopback port and start its accept loop on
/// a dedicated `std::thread`. Returns the server (for `shutdown`), its
/// address, and the accept-loop join handle.
pub fn start_server(
    db: Arc<PvDatabase>,
) -> (Arc<BlockingCaServer>, SocketAddr, thread::JoinHandle<()>) {
    let server = Arc::new(
        BlockingCaServer::bind(
            "127.0.0.1:0",
            db,
            epics_base_rs::server::access_security::new_acf_cell(None),
        )
        .expect("bind ephemeral loopback port"),
    );
    let addr = server.local_addr().expect("local_addr");
    let srv = server.clone();
    let accept = thread::spawn(move || srv.serve());
    (server, addr, accept)
}

// ---------------------------------------------------------------------------
// Circuit — a CA connection that loses nothing
// ---------------------------------------------------------------------------

/// A CA virtual circuit: the socket plus the frames that have arrived on it
/// and not yet been claimed by an expectation.
///
/// See the module docs for why the queue exists. The short version: a test that
/// waits for one of two racing replies must not destroy the other.
pub struct Circuit {
    sock: TcpStream,
    /// Frames read off the socket while looking for something else. Claimed by
    /// a later expectation, in arrival order.
    pending: VecDeque<Vec<u8>>,
}

impl Circuit {
    /// Wrap an already-connected stream. Tests that drive a *fake* peer use
    /// this directly; against a real server go through [`connect_and_handshake`].
    pub fn new(sock: TcpStream) -> Self {
        sock.set_read_timeout(Some(READ_TIMEOUT))
            .expect("set read timeout");
        Self {
            sock,
            pending: VecDeque::new(),
        }
    }

    pub fn send(&mut self, frame: &[u8]) {
        self.sock.write_all(frame).expect("write frame");
    }

    /// Wait for a frame carrying `cmmd`, keeping everything else.
    pub fn expect(&mut self, cmmd: u16, what: &str) -> Vec<u8> {
        self.expect_where(|f| cmmd_of(f) == cmmd, what)
    }

    /// Wait until a frame for **every** command in `cmmds` has arrived, and
    /// return them in that order.
    ///
    /// This is the shape to reach for whenever one request can produce more
    /// than one reply — a WRITE_NOTIFY that also fans out to a subscription,
    /// say. Asking for them one at a time would be correct too, since nothing
    /// is discarded, but naming them together documents that both are expected
    /// and makes the wait independent of which arrives first.
    pub fn expect_all(&mut self, cmmds: &[u16], what: &str) -> Vec<Vec<u8>> {
        cmmds.iter().map(|&cmmd| self.expect(cmmd, what)).collect()
    }

    /// Wait for the EVENT_CANCEL acknowledgement.
    ///
    /// C `event_cancel_reply` (`camessage.c:2002-2014`) echoes the stored
    /// EVENT_ADD request — same data_type / count / sid / sub-id — with a
    /// **zero payload**, rather than echoing the EVENT_CANCEL opcode. The zero
    /// postsize is what distinguishes the ack from a genuine monitor update
    /// still in flight, so that is what this matches on.
    pub fn expect_cancel_ack(&mut self, sub_id: u32) -> Vec<u8> {
        let frame = self.expect_where(
            |f| cmmd_of(f) == CA_PROTO_EVENT_ADD && postsize_of(f) == 0,
            "EVENT_CANCEL ack",
        );
        assert_eq!(
            u32::from_be_bytes([frame[12], frame[13], frame[14], frame[15]]),
            sub_id,
            "cancel ack carries the cancelled subscription id"
        );
        frame
    }

    /// Assert that the circuit is quiet: nothing is queued from earlier, and
    /// nothing new arrives within `dur`.
    ///
    /// The queue check is the half that a discarding reader could not perform.
    /// It means a test cannot leave an unclaimed reply behind and still call
    /// the circuit silent.
    pub fn expect_silence(&mut self, dur: Duration, what: &str) {
        if let Some(stale) = self.pending.pop_front() {
            panic!(
                "{what}: expected silence, but an unclaimed cmmd={} frame was \
                 already queued from an earlier exchange",
                cmmd_of(&stale)
            );
        }
        self.sock.set_read_timeout(Some(dur)).unwrap();
        let mut hdr = [0u8; CaHeader::SIZE];
        let outcome = self.sock.read_exact(&mut hdr);
        self.sock.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        match outcome {
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // expected: nothing on the wire
            }
            Ok(()) => panic!("{what}: expected silence, got cmmd={}", cmmd_of(&hdr)),
            Err(e) => panic!("{what}: unexpected socket error {e}"),
        }
    }

    /// The one place frames are matched. Scans what is already queued, then
    /// reads from the socket, stashing every frame that does not match so a
    /// later expectation can still claim it.
    fn expect_where(&mut self, mut want: impl FnMut(&[u8]) -> bool, what: &str) -> Vec<u8> {
        if let Some(idx) = self.pending.iter().position(|f| want(f)) {
            return self.pending.remove(idx).expect("index just found");
        }
        for _ in 0..FRAME_BUDGET {
            let frame = self.read_one_frame();
            if cmmd_of(&frame) == CA_PROTO_ERROR {
                let status = u32::from_be_bytes([frame[12], frame[13], frame[14], frame[15]]);
                panic!("{what}: server answered CA_PROTO_ERROR (ECA status {status:#x})");
            }
            if want(&frame) {
                return frame;
            }
            self.pending.push_back(frame);
        }
        panic!("{what}: no matching frame within {FRAME_BUDGET} frames");
    }

    /// Read exactly one whole CA frame (header + declared payload), leaving the
    /// socket on a frame boundary.
    fn read_one_frame(&mut self) -> Vec<u8> {
        let mut hdr = [0u8; CaHeader::SIZE];
        self.sock.read_exact(&mut hdr).expect("read frame header");
        let postsize = postsize_of(&hdr);
        let mut frame = hdr.to_vec();
        if postsize > 0 {
            let mut body = vec![0u8; postsize];
            self.sock.read_exact(&mut body).expect("read frame body");
            frame.extend_from_slice(&body);
        }
        frame
    }
}

pub fn cmmd_of(frame: &[u8]) -> u16 {
    u16::from_be_bytes([frame[0], frame[1]])
}

fn postsize_of(frame: &[u8]) -> usize {
    u16::from_be_bytes([frame[2], frame[3]]) as usize
}

/// The DBR_DOUBLE scalar in a reply payload (payload starts after the header).
pub fn payload_double(frame: &[u8]) -> f64 {
    f64::from_be_bytes([
        frame[16], frame[17], frame[18], frame[19], frame[20], frame[21], frame[22], frame[23],
    ])
}

// ---------------------------------------------------------------------------
// Frame builders
// ---------------------------------------------------------------------------

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
/// HOST_NAME out, and **both** of the server's VERSION frames in.
///
/// Two are due, and claiming only one is what a discarding reader let pass: C
/// `create_tcp_client` (`caservertask.c:1525`, mirrored at `blocking.rs:637`)
/// sends an unsolicited greeting as the server's very first frame, and the
/// dispatch handler then answers our own VERSION request. Leaving the second
/// queued would surface much later as an unexplained frame under some
/// unrelated expectation.
pub fn connect_and_handshake(addr: SocketAddr) -> Circuit {
    let mut c = Circuit::new(TcpStream::connect(addr).expect("connect to blocking server"));
    c.send(&version_frame());
    c.send(&string_payload_frame(CA_PROTO_CLIENT_NAME, "raw-e2e"));
    c.send(&string_payload_frame(CA_PROTO_HOST_NAME, "localhost"));
    let greeting = c.expect(CA_PROTO_VERSION, "unsolicited VERSION greeting");
    assert_eq!(
        u16::from_be_bytes([greeting[6], greeting[7]]),
        CA_MINOR_VERSION,
        "server advertises its CA minor version in the greeting"
    );
    let _echo = c.expect(CA_PROTO_VERSION, "VERSION answering our own");
    c
}

/// CREATE_CHAN for `pv`; returns the server-assigned sid.
///
/// Claims the ACCESS_RIGHTS frame the server sends alongside the reply. Like
/// the second VERSION above, it is not optional — it was simply invisible
/// while the reader discarded whatever it was not looking for.
pub fn create_channel(c: &mut Circuit, cid: u32, pv: &str) -> u32 {
    c.send(&create_chan_frame(cid, pv));
    let replies = c.expect_all(
        &[CA_PROTO_ACCESS_RIGHTS, CA_PROTO_CREATE_CHAN],
        "CREATE_CHAN and its ACCESS_RIGHTS",
    );
    let cc = &replies[1];
    assert_eq!(
        u32::from_be_bytes([cc[8], cc[9], cc[10], cc[11]]),
        cid,
        "CREATE_CHAN reply echoes our cid"
    );
    u32::from_be_bytes([cc[12], cc[13], cc[14], cc[15]])
}
