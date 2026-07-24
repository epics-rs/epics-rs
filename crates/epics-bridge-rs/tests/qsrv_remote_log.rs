//! QSRV source-layer `logRemote` diagnostics, end to end over a real PVA
//! connection.
//!
//! pvxs's IOC source layer has exactly three `logRemote` sites
//! (`ioc/groupsource.cpp:560`, `ioc/singlesource.cpp:129`,
//! `ioc/iocsource.cpp:447`). Each reports an option the client PRESENTED
//! but the server cannot use, WITHOUT changing the outcome: the write is
//! still dropped, the mask still falls back, the processing still stays
//! passive. The report itself is the contract — an IOID-tagged
//! `CMD_MESSAGE` Warning frame (`serverconn.cpp:146-160`).
//!
//! These tests speak raw PVA to a real `PvaServer` fronting the QSRV
//! source, so what they assert is the frame on the wire: its command, its
//! ioid, its `messageType` byte, and its text.

#![cfg(feature = "qsrv")]

// RTEMS-EXEC-MODEL-ALLOW(16): checked - these run and pass in the feature-ON suite.

use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;
use epics_bridge_rs::qsrv::{BridgeProvider, QsrvPvStore};
use epics_pva_rs::client_native::decode::Frame;
use epics_pva_rs::codec::{CMD_CREATE_CHANNEL, CMD_PUT, PvaCodec};
use epics_pva_rs::proto::{
    BitSet, ByteOrder, Command, MessageType, PvaHeader, ReadExt, Status, WriteExt,
    encode_string_into,
};
use epics_pva_rs::pvdata::encode::{
    decode_type_desc, default_value_for, encode_pv_field, encode_pv_field_with_bitset,
    encode_type_desc,
};
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarValue};
use epics_pva_rs::server_native::{PvaServer, PvaServerConfig};

const ORDER: ByteOrder = ByteOrder::Little;

// ---------------------------------------------------------------------------
// server fixture
// ---------------------------------------------------------------------------

/// Start a `PvaServer` on a random loopback port serving `provider`'s
/// records and groups through the QSRV source.
fn spawn_qsrv(provider: Arc<BridgeProvider>) -> (PvaServer, SocketAddr) {
    let pick_tcp = || {
        let l = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let pick_udp = || {
        let l = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let cfg = PvaServerConfig {
        tcp_port: pick_tcp(),
        udp_port: pick_udp(),
        ..PvaServerConfig::isolated()
    };
    let bound = SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        cfg.tcp_port,
    );
    let source = QsrvPvStore::new(provider);
    let server = PvaServer::start(Arc::new(source), cfg).expect("qsrv test server must start");
    (server, bound)
}

// ---------------------------------------------------------------------------
// raw PVA client
// ---------------------------------------------------------------------------

/// Persistent rx buffer so a burst of frames in one syscall (the
/// `CMD_MESSAGE` immediately followed by its op reply) is not truncated.
struct FrameReader {
    sock: std::net::TcpStream,
    buf: Vec<u8>,
}

impl FrameReader {
    /// Connect, drain the server's SET_BYTE_ORDER + CONNECTION_VALIDATION
    /// prologue, and answer it with an anonymous validation.
    fn connect(addr: SocketAddr) -> Self {
        use std::io::Read;
        let mut sock = std::net::TcpStream::connect(addr).expect("connect");
        sock.set_read_timeout(Some(Duration::from_millis(50))).ok();
        sock.set_nodelay(true).ok();

        let mut prelude = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline && prelude.len() < 16 {
            let mut chunk = [0u8; 256];
            match sock.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => prelude.extend_from_slice(&chunk[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => panic!("prelude read failed: {e}"),
            }
        }
        assert!(!prelude.is_empty(), "server sent no handshake prologue");

        let mut payload = Vec::new();
        payload.put_u32(0x10000, ORDER);
        payload.put_u16(32_767, ORDER);
        payload.put_u16(0, ORDER);
        encode_string_into("anonymous", ORDER, &mut payload);
        payload.put_u8(0xFF);
        let h = PvaHeader::application(
            false,
            ORDER,
            Command::ConnectionValidation.code(),
            payload.len() as u32,
        );
        let mut frame = Vec::new();
        h.write_into(&mut frame);
        frame.extend_from_slice(&payload);
        sock.write_all(&frame).expect("send CONNECTION_VALIDATION");

        let mut me = Self {
            sock,
            buf: Vec::new(),
        };
        let validated = me.read();
        assert_eq!(
            validated.header.command,
            Command::ConnectionValidated.code(),
            "expected CONNECTION_VALIDATED"
        );
        me
    }

    fn read(&mut self) -> Frame {
        use epics_pva_rs::client_native::decode::try_parse_frame;
        use std::io::Read;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(Some((frame, n))) = try_parse_frame(&self.buf) {
                self.buf.drain(..n);
                return frame;
            }
            if std::time::Instant::now() >= deadline {
                panic!("no complete frame within deadline");
            }
            let mut chunk = [0u8; 1024];
            match self.sock.read(&mut chunk) {
                Ok(0) => continue,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => panic!("frame read failed: {e}"),
            }
        }
    }

    /// Read one frame, or `None` if the server CLOSED the circuit first.
    ///
    /// `read` above loops on EOF until it times out, which cannot tell a
    /// reset apart from a slow reply. This is the pvxs `bev.reset()` probe:
    /// the server drops the connection rather than answering.
    fn read_or_closed(&mut self) -> Option<Frame> {
        use epics_pva_rs::client_native::decode::try_parse_frame;
        use std::io::Read;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(Some((frame, n))) = try_parse_frame(&self.buf) {
                self.buf.drain(..n);
                return Some(frame);
            }
            if std::time::Instant::now() >= deadline {
                panic!("neither a frame nor a close within deadline");
            }
            let mut chunk = [0u8; 1024];
            match self.sock.read(&mut chunk) {
                Ok(0) => return None,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return None,
                Err(e) => panic!("frame read failed: {e}"),
            }
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.sock.write_all(bytes).expect("send");
    }

    /// CREATE_CHANNEL → server channel id.
    fn create_channel(&mut self, name: &str) -> u32 {
        let mut body = Vec::new();
        body.put_u16(1, ORDER);
        body.put_u32(7, ORDER);
        encode_string_into(name, ORDER, &mut body);
        let h = PvaHeader::application(false, ORDER, CMD_CREATE_CHANNEL, body.len() as u32);
        let mut frame = Vec::new();
        h.write_into(&mut frame);
        frame.extend_from_slice(&body);
        self.send(&frame);

        let resp = self.read();
        assert_eq!(resp.header.command, CMD_CREATE_CHANNEL);
        let mut cur = resp.cursor();
        let _cid = cur.get_u32(ORDER).unwrap();
        let sid = cur.get_u32(ORDER).unwrap();
        assert_ne!(sid, u32::MAX, "CREATE_CHANNEL for {name} was refused");
        sid
    }
}

/// The `(ioid, messageType, text)` of a `CMD_MESSAGE` frame — pvxs
/// `ServerConn::logRemote` payload (`serverconn.cpp:151-157`).
fn parse_message_frame(frame: &Frame) -> (u32, u8, String) {
    assert_eq!(
        frame.header.command,
        Command::Message.code(),
        "expected a CMD_MESSAGE frame, got command {:#x}",
        frame.header.command
    );
    let mut cur = frame.cursor();
    let ioid = cur.get_u32(ORDER).unwrap();
    let mtype = cur.get_u8().unwrap();
    let msg = epics_pva_rs::proto::decode_string(&mut cur, ORDER)
        .expect("message string")
        .unwrap_or_default();
    (ioid, mtype, msg)
}

// ---------------------------------------------------------------------------
// pvRequest builders
// ---------------------------------------------------------------------------

/// A pvRequest carrying `record._options.<name> = <value>` (and the empty
/// `field` selector), encoded as the `desc + value` body an INIT frame
/// takes.
fn pv_request_with_options(options: &[(&str, PvField)]) -> Vec<u8> {
    let mut opts = PvStructure::new("");
    for (name, value) in options {
        opts.fields.push(((*name).into(), value.clone()));
    }
    let mut record = PvStructure::new("");
    record
        .fields
        .push(("_options".into(), PvField::Structure(opts)));

    let mut root = PvStructure::new("");
    root.fields
        .push(("record".into(), PvField::Structure(record)));
    root.fields
        .push(("field".into(), PvField::Structure(PvStructure::new(""))));

    let value = PvField::Structure(root);
    let desc = value.descriptor();
    let mut out = Vec::new();
    encode_type_desc(&desc, ORDER, &mut out);
    encode_pv_field(&value, &desc, ORDER, &mut out);
    out
}

/// PUT INIT reply body → the negotiated introspection.
fn put_init_desc(reader: &mut FrameReader, ioid: u32) -> FieldDesc {
    let frame = reader.read();
    assert_eq!(frame.header.command, CMD_PUT, "expected the PUT INIT reply");
    let mut cur = frame.cursor();
    assert_eq!(cur.get_u32(ORDER).unwrap(), ioid);
    let subcmd = cur.get_u8().unwrap();
    assert!(subcmd & 0x08 != 0, "INIT reply must echo 0x08");
    let st = Status::decode(&mut cur, ORDER).unwrap();
    assert!(st.is_success(), "PUT INIT must succeed: {st:?}");
    decode_type_desc(&mut cur, ORDER).expect("PUT INIT reply carries the introspection")
}

/// A whole-structure PUT data frame: every bit marked (bit 0 = the root),
/// so the server sees every member as client-marked — the `marked` set
/// pvxs's `putGroupField` tests.
fn put_all_marked(
    reader: &mut FrameReader,
    codec: &PvaCodec,
    sid: u32,
    ioid: u32,
    desc: &FieldDesc,
) {
    let value = default_value_for(desc);
    let changed = BitSet::all_set(desc.total_bits());
    let mut data = Vec::new();
    changed.write_into(ORDER, &mut data);
    encode_pv_field_with_bitset(&value, desc, &changed, 0, ORDER, &mut data);
    let frame = codec.build_put(sid, ioid, &data);
    reader.send(&frame);
}

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

async fn db_with_two_records() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    db.add_record("RLOG:a", Box::new(AiRecord::new(1.0)))
        .await
        .expect("add a");
    db.add_record("RLOG:b", Box::new(AiRecord::new(2.0)))
        .await
        .expect("add b");
    db
}

/// `a` is putable (`+putorder`), `b` is not — pvxs's not-putable sentinel
/// (`fieldconfig.h:37`, `groupsource.cpp:503`).
const GROUP_MIXED_PUTORDER: &str = r#"{
    "RLOG:grp": {
        "+atomic": false,
        "a": { "+channel": "RLOG:a.VAL", "+type": "plain", "+putorder": 0 },
        "b": { "+channel": "RLOG:b.VAL", "+type": "plain" }
    }
}"#;

// ---------------------------------------------------------------------------
// R7-31 — group PUT, marked member with no +putorder
// ---------------------------------------------------------------------------

/// pvxs `groupsource.cpp:556-561`: a group member the client marked in a
/// PUT but which carries no `+putorder` is `marked && !putable` — the
/// write is dropped AND the client is told, by name, with a Warn
/// `CMD_MESSAGE` on the PUT's ioid. Rust dropped it silently.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_put_unputable_marked_member_warns_over_the_wire() {
    let db = db_with_two_records().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider
        .load_group_config(GROUP_MIXED_PUTORDER)
        .expect("group config loads");
    let (_server, addr) = spawn_qsrv(provider);

    let codec = PvaCodec { big_endian: false };
    let ioid: u32 = 42;
    let mut c = FrameReader::connect(addr);
    let sid = c.create_channel("RLOG:grp");

    c.send(&codec.build_put_init(sid, ioid, &pv_request_with_options(&[])));
    let desc = put_init_desc(&mut c, ioid);
    put_all_marked(&mut c, &codec, sid, ioid, &desc);

    // pvxs logs from inside the PUT and replies afterwards: the Warning
    // frame precedes the PUT reply on the wire.
    let (msg_ioid, mtype, text) = parse_message_frame(&c.read());
    assert_eq!(msg_ioid, ioid, "the diagnostic must carry the PUT's ioid");
    assert_eq!(
        mtype,
        MessageType::Warning as u8,
        "pvxs logs this at Level::Warn"
    );
    assert_eq!(text, "b: no putorder, ignore write");

    let reply = c.read();
    assert_eq!(reply.header.command, CMD_PUT, "the PUT reply follows");
    let mut cur = reply.cursor();
    assert_eq!(cur.get_u32(ORDER).unwrap(), ioid);
    let _subcmd = cur.get_u8().unwrap();
    let st = Status::decode(&mut cur, ORDER).unwrap();
    assert!(
        st.is_success(),
        "the diagnostic does not change the outcome — the PUT still succeeds: {st:?}"
    );

    // The diagnostic reports the drop; it does not undo it. The
    // no-putorder member is still never written.
    let b = db.get_pv("RLOG:b.VAL").expect("read b");
    assert!(
        matches!(b, EpicsValue::Double(v) if v == 2.0),
        "the no-putorder member must keep its pre-PUT value, got {b:?}"
    );
}

/// Control: every member putable → no diagnostic, the PUT reply is the
/// first frame back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_put_all_putable_emits_no_message() {
    const ALL_PUTABLE: &str = r#"{
        "RLOG:grp_ok": {
            "+atomic": false,
            "a": { "+channel": "RLOG:a.VAL", "+type": "plain", "+putorder": 0 },
            "b": { "+channel": "RLOG:b.VAL", "+type": "plain", "+putorder": 1 }
        }
    }"#;

    let db = db_with_two_records().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider
        .load_group_config(ALL_PUTABLE)
        .expect("group loads");
    let (_server, addr) = spawn_qsrv(provider);

    let codec = PvaCodec { big_endian: false };
    let ioid: u32 = 43;
    let mut c = FrameReader::connect(addr);
    let sid = c.create_channel("RLOG:grp_ok");

    c.send(&codec.build_put_init(sid, ioid, &pv_request_with_options(&[])));
    let desc = put_init_desc(&mut c, ioid);
    put_all_marked(&mut c, &codec, sid, ioid, &desc);

    let reply = c.read();
    assert_eq!(
        reply.header.command, CMD_PUT,
        "a clean group PUT must not be preceded by a CMD_MESSAGE"
    );
}

/// A `+type:const` member has no backing channel, so pvxs's `marked`
/// (`leafNode.isMarked(...) && field.value`) is false for it and no
/// diagnostic is owed even though it, too, has no `+putorder`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_put_const_member_without_putorder_emits_no_message() {
    const CONST_MEMBER: &str = r#"{
        "RLOG:grp_const": {
            "+atomic": false,
            "a": { "+channel": "RLOG:a.VAL", "+type": "plain", "+putorder": 0 },
            "k": { "+type": "const", "+const": 42 }
        }
    }"#;

    let db = db_with_two_records().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider
        .load_group_config(CONST_MEMBER)
        .expect("group loads");
    let (_server, addr) = spawn_qsrv(provider);

    let codec = PvaCodec { big_endian: false };
    let ioid: u32 = 44;
    let mut c = FrameReader::connect(addr);
    let sid = c.create_channel("RLOG:grp_const");

    c.send(&codec.build_put_init(sid, ioid, &pv_request_with_options(&[])));
    let desc = put_init_desc(&mut c, ioid);
    put_all_marked(&mut c, &codec, sid, ioid, &desc);

    let reply = c.read();
    assert_eq!(
        reply.header.command, CMD_PUT,
        "a channel-less const member must not draw a no-putorder warning"
    );
}

// ---------------------------------------------------------------------------
// R7-32 — single-record MONITOR, record._options.DBE selecting an empty mask
// ---------------------------------------------------------------------------

/// Drive a MONITOR INIT + START and return every frame the server sent up
/// to (and including) the first MONITOR *data* frame.
///
/// pvxs raises its `logRemote` inside `onSubscribe`, i.e. BEFORE the INIT
/// reply its `connect()` sends. Since R10-37 the port matches that: the
/// `record._options.DBE` read is the INIT-time `check_monitor_request` hook and
/// the wire layer drains its log before building the reply. Frame ORDER is
/// therefore part of the contract — see
/// [`monitor_dbe_empty_mask_precedes_the_init_reply`].
fn monitor_until_data(
    c: &mut FrameReader,
    codec: &PvaCodec,
    sid: u32,
    ioid: u32,
    pv_request: &[u8],
) -> Vec<Frame> {
    c.send(&codec.build_monitor_init(sid, ioid, pv_request, None));
    c.send(&codec.build_monitor_start(sid, ioid));

    let mut frames = Vec::new();
    for _ in 0..6 {
        let f = c.read();
        let is_data = f.header.command == Command::Monitor.code() && {
            let mut cur = f.cursor();
            let _ioid = cur.get_u32(ORDER).unwrap();
            cur.get_u8().unwrap() & 0x08 == 0
        };
        frames.push(f);
        if is_data {
            return frames;
        }
    }
    panic!("no MONITOR data frame within 6 frames");
}

/// pvxs `singlesource.cpp:122-130`: a `record._options.DBE` string whose
/// sloppy substring parse matches none of VALUE / ARCHIVE / ALARM selects
/// an empty value mask. The subscription still falls back to VALUE|ALARM,
/// but pvxs first tells the client its selection was empty. `"LOG"` is
/// exactly that trap — an EPICS event-class name that is NOT one of the
/// three spellings pvxs matches.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monitor_dbe_empty_mask_warns_over_the_wire() {
    let db = db_with_two_records().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    let (_server, addr) = spawn_qsrv(provider);

    let codec = PvaCodec { big_endian: false };
    let ioid: u32 = 51;
    let mut c = FrameReader::connect(addr);
    let sid = c.create_channel("RLOG:a");

    let req =
        pv_request_with_options(&[("DBE", PvField::Scalar(ScalarValue::String("LOG".into())))]);
    let frames = monitor_until_data(&mut c, &codec, sid, ioid, &req);

    let messages: Vec<_> = frames
        .iter()
        .filter(|f| f.header.command == Command::Message.code())
        .map(parse_message_frame)
        .collect();
    assert_eq!(
        messages.len(),
        1,
        "exactly one empty-mask diagnostic is owed, got {messages:?}"
    );
    let (msg_ioid, mtype, text) = &messages[0];
    assert_eq!(*msg_ioid, ioid, "the diagnostic carries the MONITOR's ioid");
    assert_eq!(*mtype, MessageType::Warning as u8);
    assert_eq!(text, "record._options.DBE=\"LOG\" selects empty mask");
}

/// R10-37: the empty-mask diagnostic is an INIT-time event, so it must reach
/// the client BEFORE the MONITOR INIT reply.
///
/// pvxs records it inside `onSubscribe` (`singlesource.cpp:128-130`), which
/// runs before the `connect()` that emits the INIT reply — so a pvxs client
/// reads CMD_MESSAGE, then the INIT reply. The port used to parse DBE twice:
/// once at INIT against a discarded log (for the `NoConvert` throw only) and
/// again at START to log, which put the message AFTER the reply. Now the
/// INIT-time `check_monitor_request` hook owns both outcomes and the wire layer
/// drains its log before building the reply.
///
/// This asserts the ORDER the set-based tests above deliberately do not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monitor_dbe_empty_mask_precedes_the_init_reply() {
    let db = db_with_two_records().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    let (_server, addr) = spawn_qsrv(provider);

    let codec = PvaCodec { big_endian: false };
    let ioid: u32 = 71;
    let mut c = FrameReader::connect(addr);
    let sid = c.create_channel("RLOG:a");

    let req =
        pv_request_with_options(&[("DBE", PvField::Scalar(ScalarValue::String("LOG".into())))]);
    let frames = monitor_until_data(&mut c, &codec, sid, ioid, &req);

    let message_at = frames
        .iter()
        .position(|f| f.header.command == Command::Message.code())
        .expect("the empty-mask diagnostic is owed");
    // The MONITOR INIT reply — Command::Monitor with the INIT bit (0x08) set.
    let init_reply_at = frames
        .iter()
        .position(|f| {
            f.header.command == Command::Monitor.code() && {
                let mut cur = f.cursor();
                let _ioid = cur.get_u32(ORDER).unwrap();
                cur.get_u8().unwrap() & 0x08 != 0
            }
        })
        .expect("the MONITOR INIT reply is owed");

    assert!(
        message_at < init_reply_at,
        "pvxs logs from inside onSubscribe, before connect() sends the INIT \
         reply: the CMD_MESSAGE must precede it (message at {message_at}, \
         INIT reply at {init_reply_at})"
    );
}

/// Negative control for the ordering above: a group MONITOR carrying the same
/// empty-mask DBE draws NO diagnostic at all. pvxs serves groups through
/// `GroupSource::onSubscribe` (`ioc/groupsource.cpp:395-405`), which reads
/// `atomic` and never `DBE` — so there is nothing to report. Logging from the
/// START-time parse (as the port used to) warned here as well, for an option
/// pvxs's group source never looks at.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_monitor_empty_mask_dbe_draws_no_diagnostic() {
    let db = db_with_two_records().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider
        .load_group_config(GROUP_MIXED_PUTORDER)
        .expect("group config loads");
    let (_server, addr) = spawn_qsrv(provider);

    let codec = PvaCodec { big_endian: false };
    let ioid: u32 = 72;
    let mut c = FrameReader::connect(addr);
    let sid = c.create_channel("RLOG:grp");

    let req =
        pv_request_with_options(&[("DBE", PvField::Scalar(ScalarValue::String("LOG".into())))]);
    let frames = monitor_until_data(&mut c, &codec, sid, ioid, &req);

    let messages: Vec<_> = frames
        .iter()
        .filter(|f| f.header.command == Command::Message.code())
        .map(parse_message_frame)
        .collect();
    assert!(
        messages.is_empty(),
        "GroupSource::onSubscribe never reads DBE, so no diagnostic is owed: {messages:?}"
    );
}

/// A lowercase token is the same trap: pvxs's substring search is case
/// SENSITIVE, so `"value"` matches nothing and selects an empty mask.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monitor_dbe_lowercase_token_warns_over_the_wire() {
    let db = db_with_two_records().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    let (_server, addr) = spawn_qsrv(provider);

    let codec = PvaCodec { big_endian: false };
    let ioid: u32 = 52;
    let mut c = FrameReader::connect(addr);
    let sid = c.create_channel("RLOG:a");

    let req = pv_request_with_options(&[(
        "DBE",
        PvField::Scalar(ScalarValue::String("value|alarm".into())),
    )]);
    let frames = monitor_until_data(&mut c, &codec, sid, ioid, &req);

    let messages: Vec<_> = frames
        .iter()
        .filter(|f| f.header.command == Command::Message.code())
        .map(parse_message_frame)
        .collect();
    assert_eq!(
        messages.len(),
        1,
        "one empty-mask diagnostic, got {messages:?}"
    );
    assert_eq!(
        messages[0].2,
        "record._options.DBE=\"value|alarm\" selects empty mask"
    );
}

/// R7-34: a numeric *string* DBE is the same trap. pvxs switches on the
/// field's kind (singlesource.cpp:117-140) — `Kind::String` runs the substring
/// scan and nothing else; only `Kind::Integer`/`Kind::Real` reach
/// `fld.as<uint8_t>()`. So `DBE="1"` names no event class: empty mask, one
/// Warning, VALUE|ALARM fallback. The port used to parse the string
/// numerically, so `"1"` silently negotiated a VALUE-only subscription (ALARM
/// transitions never reached the client) and no diagnostic was owed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monitor_dbe_numeric_string_warns_over_the_wire() {
    let db = db_with_two_records().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    let (_server, addr) = spawn_qsrv(provider);

    let codec = PvaCodec { big_endian: false };
    // "1" would have been DBE_VALUE, "2" DBE_ALARM, "7" VALUE|LOG|ALARM.
    for (i, raw) in ["1", "2", "7"].iter().enumerate() {
        let ioid: u32 = 61 + i as u32;
        let mut c = FrameReader::connect(addr);
        let sid = c.create_channel("RLOG:a");

        let req = pv_request_with_options(&[(
            "DBE",
            PvField::Scalar(ScalarValue::String((*raw).into())),
        )]);
        let frames = monitor_until_data(&mut c, &codec, sid, ioid, &req);

        let messages: Vec<_> = frames
            .iter()
            .filter(|f| f.header.command == Command::Message.code())
            .map(parse_message_frame)
            .collect();
        assert_eq!(
            messages.len(),
            1,
            "DBE={raw:?} selects an empty mask and owes exactly one warning, got {messages:?}"
        );
        let (msg_ioid, mtype, text) = &messages[0];
        assert_eq!(*msg_ioid, ioid);
        assert_eq!(*mtype, MessageType::Warning as u8);
        assert_eq!(
            text,
            &format!("record._options.DBE=\"{raw}\" selects empty mask")
        );
    }
}

/// Control: the SAME value carried as a numeric field (not a string) IS read
/// as a mask — pvxs's `Kind::Integer` branch does `fld.as<uint8_t>()`
/// (singlesource.cpp:134-136). `DBE=1` (Int) is an honored VALUE selection, so
/// no diagnostic. This is the boundary the string arm must not cross.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monitor_dbe_numeric_int_is_honored_without_warning() {
    let db = db_with_two_records().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    let (_server, addr) = spawn_qsrv(provider);

    let codec = PvaCodec { big_endian: false };
    let ioid: u32 = 64;
    let mut c = FrameReader::connect(addr);
    let sid = c.create_channel("RLOG:a");

    let req = pv_request_with_options(&[("DBE", PvField::Scalar(ScalarValue::Int(1)))]);
    let frames = monitor_until_data(&mut c, &codec, sid, ioid, &req);

    assert!(
        !frames
            .iter()
            .any(|f| f.header.command == Command::Message.code()),
        "a numeric-typed DBE=1 is an honored VALUE selection and must draw no CMD_MESSAGE"
    );
}

/// Control: a DBE that DOES select something in the value class is a
/// request the server honors as asked — no diagnostic.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monitor_dbe_recognized_token_emits_no_message() {
    let db = db_with_two_records().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    let (_server, addr) = spawn_qsrv(provider);

    let codec = PvaCodec { big_endian: false };
    let ioid: u32 = 53;
    let mut c = FrameReader::connect(addr);
    let sid = c.create_channel("RLOG:a");

    let req = pv_request_with_options(&[(
        "DBE",
        PvField::Scalar(ScalarValue::String("VALUE|ALARM".into())),
    )]);
    let frames = monitor_until_data(&mut c, &codec, sid, ioid, &req);

    assert!(
        !frames
            .iter()
            .any(|f| f.header.command == Command::Message.code()),
        "an honored DBE selection must draw no CMD_MESSAGE"
    );
}

// ---------------------------------------------------------------------------
// R9-35 / CBUG-C2 — array-typed record._options.DBE fails THAT OP, not the circuit
// ---------------------------------------------------------------------------

/// pvxs `SingleSource::onSubscribe` dispatches DBE on the field's KIND and
/// then CONVERTS (`ioc/singlesource.cpp:117-140`). Kind is the type-code class
/// (`code & 0xe0`), so `Int32A` is `Kind::Integer` and reaches
/// `fld.as<uint8_t>()` — but it STORES as an array, and `Value::copyOut` has no
/// scalar arm for array storage (`data.cpp:466-499`), so it raises `NoConvert`.
///
/// DEVIATION from C++, deliberate — CBUG-C2. In QSRV that `NoConvert` escapes
/// `onSubscribe`, escapes `servermon.cpp:590-594`, and lands in
/// `conn.cpp:277-282`, which calls `bev.reset()`: no INIT reply, no monitor,
/// and the whole circuit gone. This test was
/// `monitor_dbe_array_typed_resets_the_circuit` and asserted exactly that
/// (`read_or_closed().is_none()`). A per-operation failure must not take down a
/// shared transport, so the port answers the offending MONITOR with an error
/// INIT reply and leaves the circuit up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monitor_dbe_array_typed_errors_the_op_not_the_circuit() {
    use epics_pva_rs::pvdata::TypedScalarArray;

    let db = db_with_two_records().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    let (_server, addr) = spawn_qsrv(provider);

    let codec = PvaCodec { big_endian: false };
    let ioid: u32 = 61;
    let mut c = FrameReader::connect(addr);
    let sid = c.create_channel("RLOG:a");

    let req = pv_request_with_options(&[(
        "DBE",
        PvField::ScalarArrayTyped(TypedScalarArray::Int(vec![1].into())),
    )]);
    c.send(&codec.build_monitor_init(sid, ioid, &req, None));

    let frame = c
        .read_or_closed()
        .expect("an unconvertible DBE must be answered, not answered with a hangup");
    assert_eq!(frame.header.command, Command::Monitor.code());
    let mut cur = frame.cursor();
    assert_eq!(cur.get_u32(ORDER).unwrap(), ioid);
    let subcmd = cur.get_u8().unwrap();
    assert!(subcmd & 0x08 != 0, "the error lands on the INIT reply");
    let st = Status::decode(&mut cur, ORDER).unwrap();
    assert!(!st.is_success(), "the offending MONITOR must fail: {st:?}");
}

/// The blast radius, which is the whole point of CBUG-C2: the circuit carries
/// OTHER channels, and QSRV's `bev.reset()` kills every one of them for one
/// client's field typo. Here the second channel — a plain, well-formed monitor
/// opened on the same TCP connection right after the bad one — must still be
/// served.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_dbe_does_not_disturb_the_other_channels_on_the_circuit() {
    use epics_pva_rs::pvdata::TypedScalarArray;

    let db = db_with_two_records().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    let (_server, addr) = spawn_qsrv(provider);

    let codec = PvaCodec { big_endian: false };
    let mut c = FrameReader::connect(addr);
    let bad_sid = c.create_channel("RLOG:a");
    let good_sid = c.create_channel("RLOG:b");

    // One client's typo: DBE as an array.
    let bad_req = pv_request_with_options(&[(
        "DBE",
        PvField::ScalarArrayTyped(TypedScalarArray::Int(vec![1].into())),
    )]);
    c.send(&codec.build_monitor_init(bad_sid, 71, &bad_req, None));
    let err = c.read_or_closed().expect("the bad op is answered");
    assert_eq!(err.header.command, Command::Monitor.code());

    // The neighbour on the same circuit is untouched.
    let good_req =
        pv_request_with_options(&[("DBE", PvField::Scalar(ScalarValue::String("VALUE".into())))]);
    let frames = monitor_until_data(&mut c, &codec, good_sid, 72, &good_req);
    assert!(
        frames
            .iter()
            .any(|f| f.header.command == Command::Monitor.code()),
        "the other channel on the circuit must keep working"
    );
}

/// The scope boundary. pvxs's `GroupSource::onSubscribe` (`ioc/groupsource.cpp`)
/// reads `record._options.atomic` and never `DBE`, so the SAME array-typed DBE
/// on a GROUP channel is never converted, never throws, and the monitor is
/// served normally. Resetting here would be a new divergence, not a fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monitor_dbe_array_typed_on_a_group_still_serves() {
    use epics_pva_rs::pvdata::TypedScalarArray;

    let db = db_with_two_records().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider
        .load_group_config(GROUP_MIXED_PUTORDER)
        .expect("group config loads");
    let (_server, addr) = spawn_qsrv(provider);

    let codec = PvaCodec { big_endian: false };
    let ioid: u32 = 62;
    let mut c = FrameReader::connect(addr);
    let sid = c.create_channel("RLOG:grp");

    let req = pv_request_with_options(&[(
        "DBE",
        PvField::ScalarArrayTyped(TypedScalarArray::Int(vec![1].into())),
    )]);
    let frames = monitor_until_data(&mut c, &codec, sid, ioid, &req);

    assert!(
        frames
            .iter()
            .any(|f| f.header.command == Command::Monitor.code()),
        "a group monitor never reads DBE, so it must be served, not reset"
    );
}

// ---------------------------------------------------------------------------
// R7-33 — record._options.process with an unsupported value
// ---------------------------------------------------------------------------

/// Run a single-record PUT INIT + PUT with `record._options.process` set
/// to `value`, and return every frame up to and including the PUT reply.
fn put_with_process(
    c: &mut FrameReader,
    codec: &PvaCodec,
    sid: u32,
    ioid: u32,
    value: PvField,
) -> Vec<Frame> {
    let req = pv_request_with_options(&[("process", value)]);
    c.send(&codec.build_put_init(sid, ioid, &req));
    let desc = put_init_desc(c, ioid);
    put_all_marked(c, codec, sid, ioid, &desc);

    let mut frames = Vec::new();
    for _ in 0..4 {
        let f = c.read();
        let is_reply = f.header.command == CMD_PUT;
        frames.push(f);
        if is_reply {
            return frames;
        }
    }
    panic!("no PUT reply within 4 frames");
}

/// pvxs `iocsource.cpp:436-447`: `record._options.process` is read with
/// `as<bool>`; a value that fails that AND is not the literal `"passive"`
/// is "unsupported" — processing stays at the passive default and pvxs
/// names the option and its value to the client. Rust collapsed it into a
/// silent passive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn put_unsupported_process_option_warns_over_the_wire() {
    let db = db_with_two_records().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    let (_server, addr) = spawn_qsrv(provider);

    let codec = PvaCodec { big_endian: false };
    let ioid: u32 = 61;
    let mut c = FrameReader::connect(addr);
    let sid = c.create_channel("RLOG:a");

    let frames = put_with_process(
        &mut c,
        &codec,
        sid,
        ioid,
        PvField::Scalar(ScalarValue::String("bogus".into())),
    );

    let messages: Vec<_> = frames
        .iter()
        .filter(|f| f.header.command == Command::Message.code())
        .map(parse_message_frame)
        .collect();
    assert_eq!(messages.len(), 1, "one diagnostic owed, got {messages:?}");
    let (msg_ioid, mtype, text) = &messages[0];
    assert_eq!(*msg_ioid, ioid, "the diagnostic carries the PUT's ioid");
    assert_eq!(*mtype, MessageType::Warning as u8);
    // pvxs streams the offending Value through its default (tree)
    // formatter — `SB()<<proc` yields `string = "bogus"` plus the
    // formatter's trailing newline (datafmt.cpp:187-211).
    assert_eq!(
        text, "Ignoring unsupported record._options.process: string = \"bogus\"\n",
        "the message names the option AND renders its value like pvxs"
    );

    // The diagnostic does not change the outcome: the PUT still lands.
    let reply = frames.last().expect("PUT reply");
    assert_eq!(reply.header.command, CMD_PUT);
    let mut cur = reply.cursor();
    assert_eq!(cur.get_u32(ORDER).unwrap(), ioid);
    let _subcmd = cur.get_u8().unwrap();
    let st = Status::decode(&mut cur, ORDER).unwrap();
    assert!(st.is_success(), "PUT still succeeds: {st:?}");
}

/// The distinction R7-33 restores: an explicit `"passive"` is a SUPPORTED
/// spelling of the default (pvxs `iocsource.cpp:440-443` returns before
/// the log) and must stay silent, even though it maps to the same
/// ProcessMode as the unsupported value above.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn put_explicit_passive_process_option_emits_no_message() {
    let db = db_with_two_records().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    let (_server, addr) = spawn_qsrv(provider);

    let codec = PvaCodec { big_endian: false };
    let ioid: u32 = 62;
    let mut c = FrameReader::connect(addr);
    let sid = c.create_channel("RLOG:a");

    let frames = put_with_process(
        &mut c,
        &codec,
        sid,
        ioid,
        PvField::Scalar(ScalarValue::String("passive".into())),
    );
    assert_eq!(
        frames.len(),
        1,
        "explicit \"passive\" is supported — the PUT reply must be the only frame"
    );
    assert_eq!(frames[0].header.command, CMD_PUT);
}

/// Control: a value `as<bool>` accepts (here a bool-typed `true`) is
/// honored, not warned about.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn put_bool_process_option_emits_no_message() {
    let db = db_with_two_records().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    let (_server, addr) = spawn_qsrv(provider);

    let codec = PvaCodec { big_endian: false };
    let ioid: u32 = 63;
    let mut c = FrameReader::connect(addr);
    let sid = c.create_channel("RLOG:a");

    let frames = put_with_process(
        &mut c,
        &codec,
        sid,
        ioid,
        PvField::Scalar(ScalarValue::Boolean(true)),
    );
    assert_eq!(
        frames.len(),
        1,
        "an as<bool>-convertible process option draws no CMD_MESSAGE"
    );
    assert_eq!(frames[0].header.command, CMD_PUT);
}
