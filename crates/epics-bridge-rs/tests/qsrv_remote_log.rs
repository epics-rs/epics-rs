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
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure};
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
    let b = db.get_pv("RLOG:b.VAL").await.expect("read b");
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
