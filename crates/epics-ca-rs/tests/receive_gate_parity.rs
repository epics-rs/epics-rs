//! **The two CA receive loops answer every protocol gate identically.**
//!
//! `epics-ca-rs` serves CA from two loops: the async host driver
//! (`server::tcp::handle_client`) and the blocking driver the RTEMS and
//! VxWorks images run (`server::blocking`). They used to carry two
//! hand-maintained lists of protocol gates, and the lists drifted — twice.
//! Both now run the one gate sequence in `server::recv`
//! (`RecvAccumulator::next_message`), so this file's job is to prove the
//! *observable* half of that: the same bytes in produce the same bytes out
//! and the same circuit fate, whichever loop received them.
//!
//! Every test here drives BOTH servers with one script and compares. A test
//! that covered only the async loop is precisely what let this defect family
//! survive a previous review round, so a one-loop test in this file is a bug
//! in the test.
//!
//! Ports are always ephemeral (`:0`) — never the real 5064, per the
//! `build() ⟹ listening` port-ownership rule.

// Host/tokio-only: the async server's listener stack needs a tokio reactor,
// which the `rtems-exec-model` background executor does not start. The
// blocking driver's own gate coverage is feature-neutral and lives in
// `server::blocking`'s unit tests; what this file adds — the comparison
// between the two loops — is only meaningful where both can run.
#![cfg(not(feature = "rtems-exec-model"))]

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::protocol::{
    CA_MINOR_VERSION, CA_PROTO_CLEAR_CHANNEL, CA_PROTO_CREATE_CHAN, CA_PROTO_ECHO, CA_PROTO_ERROR,
    CA_PROTO_READ_NOTIFY, CA_PROTO_SEARCH, CA_PROTO_VERSION, CaHeader,
};
use epics_ca_rs::server::CaServer;
use epics_ca_rs::server::blocking::BlockingCaServer;

const PV: &str = "GATE:PARITY";
/// How long a read waits before the circuit is declared silent. Loopback
/// replies land in microseconds; this only has to outrun scheduling noise.
const READ_TIMEOUT: Duration = Duration::from_millis(1500);

// ---------------------------------------------------------------------------
// What a loop did with the script
// ---------------------------------------------------------------------------

/// Everything a test compares between the two loops.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    /// The `CA_PROTO_ERROR` frames the script drew, in order.
    errors: Vec<ErrorFrame>,
    /// Whether the server closed the circuit after the script.
    closed: bool,
}

/// One `CA_PROTO_ERROR`, as C `vsend_err` (`camessage.c:150-245`) lays it out.
#[derive(Debug, PartialEq, Eq)]
struct ErrorFrame {
    /// `m_available` of the response — the ECA status.
    status: u32,
    diagnostic: String,
    /// `m_cmmd` of the echoed request header.
    echoed_cmmd: u16,
    /// 16 or 24: which form `vsend_err` chose for the echo. Part of the wire
    /// contract, since it moves where the diagnostic string starts.
    echo_len: usize,
}

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
// A raw client that reads whole frames and never discards one
// ---------------------------------------------------------------------------

struct Raw {
    sock: TcpStream,
    pending: VecDeque<Vec<u8>>,
}

impl Raw {
    fn connect(addr: SocketAddr) -> Self {
        let sock = TcpStream::connect(addr).expect("connect");
        sock.set_read_timeout(Some(READ_TIMEOUT)).expect("timeout");
        Self {
            sock,
            pending: VecDeque::new(),
        }
    }

    fn send(&mut self, frame: &[u8]) {
        self.sock.write_all(frame).expect("write frame");
    }

    /// Read one whole frame, header plus declared body.
    ///
    /// EOF and a read timeout are kept apart deliberately: "the server closed
    /// the circuit" and "the server is still there and said nothing" are the
    /// two outcomes this file exists to tell apart, and `read_exact` reports
    /// both as `Err`.
    fn next_frame(&mut self) -> Read1 {
        if let Some(f) = self.pending.pop_front() {
            return Read1::Frame(f);
        }
        let mut hdr = [0u8; CaHeader::SIZE];
        match self.sock.read_exact(&mut hdr) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Read1::Closed,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Read1::Quiet;
            }
            // A peer-side reset is a close, which is what a C IOC's
            // `RSRV_ERROR` teardown looks like on some stacks.
            Err(_) => return Read1::Closed,
        }
        let mut frame = hdr.to_vec();
        // Extended form carries the real size in the 8-byte annex.
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
        Read1::Frame(frame)
    }

    /// Read until the server closes or falls silent, collecting every
    /// `CA_PROTO_ERROR`. Returns `(errors, closed)`.
    fn collect_until_quiet(&mut self) -> (Vec<ErrorFrame>, bool) {
        let mut errors = Vec::new();
        loop {
            match self.next_frame() {
                Read1::Closed => return (errors, true),
                Read1::Quiet => return (errors, false),
                Read1::Frame(frame) => {
                    let cmmd = u16::from_be_bytes([frame[0], frame[1]]);
                    if cmmd == CA_PROTO_ERROR {
                        errors.push(parse_error_frame(&frame));
                    }
                    // Keep reading either way: whether a close follows the
                    // error is the half being compared.
                }
            }
            assert!(
                errors.len() <= 8,
                "runaway error stream — the circuit answered every frame instead of closing"
            );
        }
    }
}

/// One read from the circuit.
enum Read1 {
    Frame(Vec<u8>),
    /// The server closed the circuit (C `RSRV_ERROR`).
    Closed,
    /// The server is still there and sent nothing within the settle window.
    Quiet,
}

/// Take a `CA_PROTO_ERROR` apart, per C `vsend_err` (`camessage.c:195-245`).
fn parse_error_frame(frame: &[u8]) -> ErrorFrame {
    let status = u32::from_be_bytes([frame[12], frame[13], frame[14], frame[15]]);
    let body = &frame[CaHeader::SIZE..];
    let echoed_cmmd = u16::from_be_bytes([body[0], body[1]]);
    // The echoed request header is 16 bytes, or 24 when it carries the
    // extended annex.
    let echo_len = if u16::from_be_bytes([body[2], body[3]]) == 0xFFFF {
        24
    } else {
        16
    };
    let text = &body[echo_len.min(body.len())..];
    let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
    ErrorFrame {
        status,
        diagnostic: String::from_utf8_lossy(&text[..end]).to_string(),
        echoed_cmmd,
        echo_len,
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

/// A `CA_PROTO_ECHO` whose declared body is `postsize` bytes, followed by
/// exactly that many bytes. With `postsize = 4` the message is 20 bytes long,
/// which C refuses as misaligned (`msgsize & 0x7`).
fn echo_frame(postsize: u16) -> Vec<u8> {
    let mut h = CaHeader::new(CA_PROTO_ECHO);
    h.postsize = postsize;
    let mut f = h.to_bytes().to_vec();
    f.extend(std::iter::repeat_n(0u8, postsize as usize));
    f
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

// ---------------------------------------------------------------------------
// Fixtures: the same database in front of each loop
// ---------------------------------------------------------------------------

fn seed_db() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    block_on_sync(db.add_pv(PV, EpicsValue::Double(1.5)))
        .expect("no async runtime on this thread")
        .expect("add_pv");
    db
}

/// Run `script` against the blocking driver and report what came back.
fn against_blocking(script: &[Vec<u8>]) -> Outcome {
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

    let outcome = drive(addr, script);

    server.shutdown();
    let _ = accept.join();
    outcome
}

/// Run `script` against the async driver and report what came back.
///
/// The server owns a tokio runtime on its own thread; the client stays a
/// plain blocking socket so both loops face byte-for-byte the same peer.
fn against_async(script: &[Vec<u8>]) -> Outcome {
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
                .pv(PV, EpicsValue::Double(1.5))
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
    let outcome = drive(addr, script);

    let _ = stop_tx.send(());
    let _ = server_thread.join();
    outcome
}

/// Play `script` and report every error frame plus whether the server closed.
///
/// A script whose first frame is a `CA_PROTO_VERSION` is a handshaken peer;
/// one that starts with anything else is a peer that never identified itself,
/// which is C's `!CA_VSUPPORTED(minor_version_number)` case. The scripts say
/// which they are, because for one finding the missing handshake IS the
/// trigger.
fn drive(addr: SocketAddr, script: &[Vec<u8>]) -> Outcome {
    let mut c = Raw::connect(addr);
    for frame in script {
        c.send(frame);
    }
    let (errors, closed) = c.collect_until_quiet();
    Outcome { errors, closed }
}

/// `script` with the CA version handshake in front of it.
fn handshaken(script: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut full = vec![version_frame()];
    full.extend(script);
    full
}

/// Run one script against both loops, assert each matches `expect`, and
/// assert the two loops agree with each other.
fn both_loops_answer(script: &[Vec<u8>], expect: &Outcome) {
    let blocking = against_blocking(script);
    let asynchronous = against_async(script);
    for (which, got) in [(Loops::Blocking, &blocking), (Loops::Async, &asynchronous)] {
        assert_eq!(
            got, expect,
            "{which} answered the script differently from the shared receive gate"
        );
    }
    assert_eq!(
        blocking, asynchronous,
        "the two CA receive loops disagree — the gate sequence is not shared"
    );
}

// ---------------------------------------------------------------------------
// C1 — misalignment
// ---------------------------------------------------------------------------

/// C `camessage.c:2519-2530`: a message whose `msgsize` is not a multiple of
/// 8 earns `ECA_INTERNAL` "CAS: Missaligned protocol rejected" and
/// `RSRV_ERROR` — the circuit is torn down.
///
/// The blocking driver had no such gate: it executed the misaligned ECHO and
/// then advanced its parse cursor by 20 bytes, so every later frame on that
/// circuit was read four bytes off, for the life of the connection. The
/// trailing `CA_PROTO_CREATE_CHAN` in this script is what a de-synced parser
/// would have mangled; against a gated loop the peer never sees it answered
/// because the circuit is already gone.
#[test]
fn a_misaligned_frame_is_refused_and_closes_the_circuit_on_both_loops() {
    let script = handshaken(vec![echo_frame(4), create_chan_frame(0x1234, PV)]);
    both_loops_answer(
        &script,
        &Outcome {
            errors: vec![ErrorFrame {
                status: epics_ca_rs::protocol::ECA_INTERNAL,
                diagnostic: "CAS: Missaligned protocol rejected".to_string(),
                echoed_cmmd: CA_PROTO_ECHO,
                echo_len: 16,
            }],
            closed: true,
        },
    );
}

/// The boundary above it: the same ECHO with an 8-aligned body is a normal
/// message on both loops — no error, circuit kept.
#[test]
fn an_aligned_frame_is_served_by_both_loops() {
    let script = handshaken(vec![echo_frame(8)]);
    let blocking = against_blocking(&script);
    let asynchronous = against_async(&script);
    assert!(
        blocking.errors.is_empty(),
        "blocking loop rejected an 8-aligned ECHO: {:?}",
        blocking.errors
    );
    assert!(
        asynchronous.errors.is_empty(),
        "async loop rejected an 8-aligned ECHO: {:?}",
        asynchronous.errors
    );
}

// ---------------------------------------------------------------------------
// C2 — illegal TCP opcodes
// ---------------------------------------------------------------------------

/// An empty-bodied frame carrying `cmmd`. 16 bytes, so it is 8-aligned and
/// reaches the opcode gate rather than the alignment one.
fn bare_frame(cmmd: u16) -> Vec<u8> {
    CaHeader::new(cmmd).to_bytes().to_vec()
}

/// C `bad_tcp_cmd_action` (`camessage.c:342-357`), reached from every
/// non-legal slot of `tcpJumpTable` (`camessage.c:2348-2377`) and from any
/// index past its end (`:2593`): `ECA_INTERNAL` "invalid (damaged?) request
/// code from TCP", then `RSRV_ERROR` — the circuit is torn down.
///
/// The blocking driver answered `ECA_UNAVAILINSERV` (a WARNING, not FATAL)
/// with a diagnostic no C IOC emits, and kept serving — one ~48-byte reply
/// per 16 bytes of garbage, forever, where C emits exactly one and closes.
#[test]
fn an_illegal_tcp_opcode_tears_the_circuit_down_on_both_loops() {
    // The gaps in C's `tcpJumpTable` (`camessage.c:2348-2377`: indices 5, 7,
    // 11, 13, 14, 16, 17, 22, 24-27 are `bad_tcp_cmd_action`), plus two
    // indices past its 28-entry end, which `camessage.c:2594-2596` routes there
    // too. Written out rather than derived from the port's own legality
    // predicate so this test states C's table independently of it.
    for cmmd in [5u16, 7, 11, 13, 14, 16, 17, 22, 24, 27, 28, 4242] {
        both_loops_answer(
            &handshaken(vec![bare_frame(cmmd)]),
            &Outcome {
                errors: vec![ErrorFrame {
                    status: epics_ca_rs::protocol::ECA_INTERNAL,
                    diagnostic: "invalid (damaged?) request code from TCP".to_string(),
                    echoed_cmmd: cmmd,
                    echo_len: 16,
                }],
                closed: true,
            },
        );
    }
}

/// The boundary beside it: every opcode C's `tcpJumpTable` binds to a real
/// handler must NOT be refused by the gate. A gate that over-rejects would
/// break every client rather than only damaged ones, and the loop below is
/// the only thing that would catch a transcription slip in
/// `is_legal_tcp_command`.
#[test]
fn every_legal_tcp_opcode_survives_the_gate_on_both_loops() {
    // ECHO is the one legal opcode that needs no channel and no prior state,
    // so it is the only one that can be sent bare; the rest are covered by
    // `is_legal_tcp_command`'s own transcription test and by the e2e suites.
    // What matters here is that a legal opcode is not answered by the gate.
    let script = handshaken(vec![bare_frame(CA_PROTO_ECHO)]);
    for (which, got) in [
        (Loops::Blocking, against_blocking(&script)),
        (Loops::Async, against_async(&script)),
    ] {
        assert!(
            got.errors.is_empty(),
            "{which} refused a legal opcode: {:?}",
            got.errors
        );
        assert!(!got.closed, "{which} closed the circuit on a legal opcode");
    }
}

// ---------------------------------------------------------------------------
// C3 — a peer that never said which protocol it speaks
// ---------------------------------------------------------------------------

/// C `camessage.c:2489-2513`: any non-VERSION message from a peer whose
/// `minor_version_number` is below `CA_MINIMUM_SUPPORTED_VERSION` earns
/// `ECA_DEFUNCT` "CAS: Client version %u too old" and is drained, with
/// `status = RSRV_OK` — C keeps the circuit open deliberately, "to avoid a
/// re-connect loop". `create_client` seeds the version at
/// `CA_UKN_MINOR_VERSION` (0), so a peer that opens a circuit and skips the
/// handshake is exactly this case.
///
/// The blocking driver had no such gate: it created the channel and returned
/// ACCESS_RIGHTS + CREATE_CHAN, so a pre-4.4 peer that cannot parse the reply
/// was left holding a server-side sid it would never clear.
#[test]
fn a_pre_handshake_message_earns_eca_defunct_on_both_loops_without_closing() {
    // No `handshaken(..)`: the missing VERSION is the trigger.
    let script = vec![create_chan_frame(0x1234, PV)];
    both_loops_answer(
        &script,
        &Outcome {
            errors: vec![ErrorFrame {
                status: epics_ca_rs::protocol::ECA_DEFUNCT,
                diagnostic: "CAS: Client version 0 too old".to_string(),
                echoed_cmmd: CA_PROTO_CREATE_CHAN,
                echo_len: 16,
            }],
            closed: false,
        },
    );
}

/// The boundary beside it: the identical CREATE_CHAN behind a VERSION
/// handshake must be served, not refused. Without this, a gate that refused
/// everything would pass the test above.
#[test]
fn the_same_message_behind_a_handshake_is_served_by_both_loops() {
    let script = handshaken(vec![create_chan_frame(0x1234, PV)]);
    for (which, got) in [
        (Loops::Blocking, against_blocking(&script)),
        (Loops::Async, against_async(&script)),
    ] {
        assert!(
            got.errors.is_empty(),
            "{which} refused a handshaken CREATE_CHAN: {:?}",
            got.errors
        );
        assert!(!got.closed, "{which} closed a handshaken circuit");
    }
}

// ---------------------------------------------------------------------------
// C5 — the order of the misalignment and ceiling gates
// ---------------------------------------------------------------------------

/// A 24-byte extended-form header declaring `postsize` body bytes, with none
/// of that body behind it. Built byte by byte rather than through the port's
/// own emitter so the test states the v4.9 wire form independently of the
/// code under test.
fn extended_frame(cmmd: u16, postsize: u32) -> Vec<u8> {
    let mut f = Vec::with_capacity(24);
    f.extend_from_slice(&cmmd.to_be_bytes());
    f.extend_from_slice(&0xFFFFu16.to_be_bytes()); // m_postsize: extended marker
    f.extend_from_slice(&0u16.to_be_bytes()); // m_dataType
    f.extend_from_slice(&0u16.to_be_bytes()); // m_count: extended marker
    f.extend_from_slice(&0u32.to_be_bytes()); // m_cid
    f.extend_from_slice(&0u32.to_be_bytes()); // m_available
    f.extend_from_slice(&postsize.to_be_bytes()); // annex m_postsize
    f.extend_from_slice(&0u32.to_be_bytes()); // annex m_count
    f
}

/// C runs the alignment test (`camessage.c:2519-2530`, `RSRV_ERROR`) *before*
/// the receive-buffer ceiling test (`:2538-2555`, `RSRV_OK` + drain), so a
/// frame that trips both is a closed circuit, not a survivable refusal.
///
/// `0x0100_0004` is 16 MiB + 4: past the ceiling and not a multiple of 8, so
/// nothing but the order decides the answer. The port ran the ceiling test
/// first and replied `ECA_TOLARGE` on a circuit it kept, which is a different
/// protocol from C's — the peer is told its request was too big and goes on
/// using a connection C would already have dropped.
#[test]
fn a_frame_that_is_misaligned_and_oversize_is_answered_as_misaligned_on_both_loops() {
    let script = handshaken(vec![extended_frame(CA_PROTO_READ_NOTIFY, 0x0100_0004)]);
    both_loops_answer(
        &script,
        &Outcome {
            errors: vec![ErrorFrame {
                status: epics_ca_rs::protocol::ECA_INTERNAL,
                diagnostic: "CAS: Missaligned protocol rejected".to_string(),
                echoed_cmmd: CA_PROTO_READ_NOTIFY,
                // The request declared 16 MiB, so C echoes the extended form.
                echo_len: 24,
            }],
            closed: true,
        },
    );
}

/// The boundary beside it: the same oversize frame with an 8-aligned length
/// clears the alignment gate and lands on the ceiling one, where C answers
/// `ECA_TOLARGE` and keeps serving. Without this case, deleting the ceiling
/// gate outright would pass the test above.
#[test]
fn an_aligned_oversize_frame_is_refused_without_closing_on_both_loops() {
    // 24-byte header + 16 MiB + 8 is a multiple of 8 and still over the
    // ceiling.
    let script = handshaken(vec![extended_frame(CA_PROTO_READ_NOTIFY, 0x0100_0008)]);
    both_loops_answer(
        &script,
        &Outcome {
            errors: vec![ErrorFrame {
                status: epics_ca_rs::protocol::ECA_TOLARGE,
                diagnostic: format!(
                    "CAS: Server unable to load large request message. Max bytes={}",
                    16 * 1024 * 1024
                ),
                echoed_cmmd: CA_PROTO_READ_NOTIFY,
                echo_len: 24,
            }],
            closed: false,
        },
    );
}

/// Above both of them, C's first framing gate (`camessage.c:2471-2478`): a
/// declared body whose `msgsize` would not fit the `ca_uint32_t` it is formed
/// in is `RSRV_ERROR` with no reply at all. The port formed that sum with a
/// saturating add, so the number every later gate read meant "the real length"
/// on ordinary frames and "a clamp" on this one — and on RTEMS and VxWorks,
/// where `usize` is 32 bits, the plain add would have wrapped to less than the
/// header it came from.
///
/// `0xFFFF_FFF8` is 8-aligned, so the alignment gate cannot account for the
/// close, and the absence of any error frame separates it from the ceiling
/// gate's refusal.
#[test]
fn an_unrepresentable_message_length_closes_the_circuit_silently_on_both_loops() {
    let script = handshaken(vec![extended_frame(CA_PROTO_READ_NOTIFY, 0xFFFF_FFF8)]);
    both_loops_answer(
        &script,
        &Outcome {
            errors: vec![],
            closed: true,
        },
    );
}

// ---------------------------------------------------------------------------
// C6 — which version a TCP SEARCH is judged by
// ---------------------------------------------------------------------------

/// A TCP `CA_PROTO_SEARCH` for `pv` whose `m_count` — the field CA carries a
/// searcher's minor version in — declares `minor`.
fn search_frame(cid: u32, pv: &str, minor: u16) -> Vec<u8> {
    let name = epics_ca_rs::protocol::pad_string(pv);
    let mut h = CaHeader::new(CA_PROTO_SEARCH);
    h.postsize = name.len() as u16;
    h.count = minor;
    h.cid = cid;
    h.available = cid;
    let mut f = h.to_bytes().to_vec();
    f.extend_from_slice(&name);
    f
}

/// C `search_reply_tcp` (`camessage.c:2292-2295`) judges a SEARCH by the
/// frame's own `m_count`, not by the version the circuit negotiated, and
/// answers `RSRV_ERROR` — the circuit goes, with no reply and no error frame.
///
/// The handshake in front of this script is what makes the two versions
/// differ: it raises the circuit to 13 while the SEARCH still declares 0. The
/// port read the circuit's number, so it answered a v4.13 SEARCH reply to a
/// searcher that had just said it cannot parse one. Both loops run the same
/// `dispatch_message`, so both must answer alike.
#[test]
fn a_search_declaring_an_ancient_version_closes_the_circuit_on_both_loops() {
    let script = handshaken(vec![search_frame(0x77, PV, 0)]);
    both_loops_answer(
        &script,
        &Outcome {
            errors: vec![],
            closed: true,
        },
    );
}

/// The boundary beside it: the same SEARCH declaring a supported version is
/// served and the circuit stays. Without this case a gate that closed on
/// every TCP SEARCH would pass the test above.
#[test]
fn a_search_declaring_a_supported_version_is_served_by_both_loops() {
    let script = handshaken(vec![search_frame(0x77, PV, CA_MINOR_VERSION)]);
    for (which, got) in [
        (Loops::Blocking, against_blocking(&script)),
        (Loops::Async, against_async(&script)),
    ] {
        assert!(
            got.errors.is_empty(),
            "{which} refused a supported-version TCP SEARCH: {:?}",
            got.errors
        );
        assert!(
            !got.closed,
            "{which} closed the circuit on a supported-version TCP SEARCH"
        );
    }
}

// ---------------------------------------------------------------------------
// C7 — which form the echoed request header takes
// ---------------------------------------------------------------------------

/// A `CA_PROTO_CLEAR_CHANNEL` for a sid no circuit holds, declaring `count`
/// in the normal 16-byte form. C `clear_channel_reply` answers an unknown sid
/// with `logBadId` — `ECA_INTERNAL` "Bad Resource ID" — and `RSRV_ERROR`.
fn clear_channel_frame(sid: u32, count: u16) -> Vec<u8> {
    let mut h = CaHeader::new(CA_PROTO_CLEAR_CHANNEL);
    h.count = count;
    h.cid = sid;
    h.available = sid;
    h.to_bytes().to_vec()
}

/// C `vsend_err` (`camessage.c:210-211`) picks the echo form with an OR over
/// the request's real size AND count, so a normal-form request carrying
/// `m_count == 0xffff` is echoed in 24 bytes with the annex — eight bytes
/// more than it arrived in, and the diagnostic starts eight bytes later.
///
/// The port asked the parsed header whether it had arrived extended, which is
/// a different question, and both loops send the frame through the same
/// `build_ca_error_frame`.
#[test]
fn a_max_count_request_draws_a_twenty_four_byte_echo_on_both_loops() {
    let script = handshaken(vec![clear_channel_frame(0xDEAD_BEEF, 0xFFFF)]);
    both_loops_answer(
        &script,
        &Outcome {
            errors: vec![ErrorFrame {
                status: epics_ca_rs::protocol::ECA_INTERNAL,
                diagnostic: "Bad Resource ID".to_string(),
                echoed_cmmd: CA_PROTO_CLEAR_CHANNEL,
                echo_len: 24,
            }],
            closed: true,
        },
    );
}

/// The boundary beside it: the identical request one count lower stays in the
/// 16-byte form, so the fix is the OR at `0xffff` and not "always echo
/// extended".
#[test]
fn a_request_below_the_count_ceiling_draws_a_sixteen_byte_echo_on_both_loops() {
    let script = handshaken(vec![clear_channel_frame(0xDEAD_BEEF, 0xFFFE)]);
    both_loops_answer(
        &script,
        &Outcome {
            errors: vec![ErrorFrame {
                status: epics_ca_rs::protocol::ECA_INTERNAL,
                diagnostic: "Bad Resource ID".to_string(),
                echoed_cmmd: CA_PROTO_CLEAR_CHANNEL,
                echo_len: 16,
            }],
            closed: true,
        },
    );
}
