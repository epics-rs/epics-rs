//! A PROCESS (command 16) request on a live channel, against pvxs 1.5.3
//! softIocPVX and against the rs server.
//!
//! pvxs has no PROCESS handler: `ConnBase::bevRead`'s default arm
//! (`conn.cpp:248-253`) logs "Ignore unexpected command", drops the segment,
//! sends nothing and keeps the connection. An ECHO sent after it is
//! therefore the next reply on the circuit. ACL_CHANGE (6), MULTIPLE_DATA
//! (19) and ORIGIN_TAG (22) reach the same arm.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use epics_pva_rs::proto::{ByteOrder, Command, PvaHeader, ReadExt, WriteExt, encode_string_into};

use super::interop_helpers::{
    PvxsLogs, SOFT_IOC_PVX, ioc_ready, pick_localhost_port, require_pvxs, soft_ioc_pvx_command,
    start_pvxs,
};

const PV: &str = "CMD16:AI";

/// The commands `bevRead` has no case for, sent after the PROCESS INIT.
const UNHANDLED: [Command; 3] = [
    Command::AclChange,
    Command::MultipleData,
    Command::OriginTag,
];

/// `conn.cpp:250`'s lines for a client at `peer`.
fn ignore_lines(peer: SocketAddr) -> Vec<String> {
    std::iter::once(Command::Process)
        .chain(UNHANDLED)
        .map(|c| {
            format!(
                "DEBUG pvxs.tcp.io Client {peer} Ignore unexpected command 0x{:02x}",
                c.code()
            )
        })
        .collect()
}

fn send(stream: &mut TcpStream, command: Command, order: ByteOrder, payload: &[u8]) {
    let mut frame = Vec::with_capacity(PvaHeader::SIZE + payload.len());
    PvaHeader::application(false, order, command.code(), payload.len() as u32)
        .write_into(&mut frame);
    frame.extend_from_slice(payload);
    stream.write_all(&frame).expect("write frame");
}

/// The next application frame's command and body; control frames are skipped.
fn next_app_frame(stream: &mut TcpStream, order: ByteOrder) -> (u8, Vec<u8>) {
    loop {
        let mut hdr = [0u8; PvaHeader::SIZE];
        stream.read_exact(&mut hdr).expect("read frame header");
        let len = match order {
            ByteOrder::Big => u32::from_be_bytes(hdr[4..8].try_into().unwrap()),
            ByteOrder::Little => u32::from_le_bytes(hdr[4..8].try_into().unwrap()),
        };
        if hdr[2] & 0x01 != 0 {
            continue;
        }
        let mut body = vec![0u8; len as usize];
        stream.read_exact(&mut body).expect("read frame body");
        return (hdr[3], body);
    }
}

/// SET_BYTE_ORDER, CONNECTION_VALIDATION with `anonymous` and a null
/// credential, CONNECTION_VALIDATED.
fn handshake(stream: &mut TcpStream) -> ByteOrder {
    let mut prelude = [0u8; PvaHeader::SIZE];
    stream
        .read_exact(&mut prelude)
        .expect("read SET_BYTE_ORDER");
    let order = if prelude[2] & 0x80 != 0 {
        ByteOrder::Big
    } else {
        ByteOrder::Little
    };
    let (cmd, _) = next_app_frame(stream, order);
    assert_eq!(cmd, Command::ConnectionValidation.code());
    let mut reply = Vec::new();
    reply.put_u32(0x10000, order);
    reply.put_u16(32_767, order);
    reply.put_u16(0, order);
    encode_string_into("anonymous", order, &mut reply);
    reply.put_u8(0xFF);
    send(stream, Command::ConnectionValidation, order, &reply);
    let (cmd, _) = next_app_frame(stream, order);
    assert_eq!(cmd, Command::ConnectionValidated.code());
    order
}

/// The application replies the server sends after a PROCESS INIT and the
/// `UNHANDLED` frames on a connected channel, before the answer to an ECHO
/// sent after them, and the client's own address.
fn replies_to_cmd16(addr: SocketAddr) -> (Vec<u8>, SocketAddr) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    let me = stream.local_addr().expect("local addr");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let order = handshake(&mut stream);

    let mut create = Vec::new();
    create.put_u16(1, order);
    create.put_u32(1, order);
    encode_string_into(PV, order, &mut create);
    send(&mut stream, Command::CreateChannel, order, &create);
    let sid = loop {
        let (cmd, body) = next_app_frame(&mut stream, order);
        if cmd == Command::CreateChannel.code() {
            let mut cur = std::io::Cursor::new(&body[..]);
            let _cid = cur.get_u32(order).expect("cid");
            break cur.get_u32(order).expect("sid");
        }
    };

    // sid, ioid, INIT, and an empty-structure pvRequest.
    let mut process = Vec::new();
    process.put_u32(sid, order);
    process.put_u32(7, order);
    process.put_u8(0x08);
    process.extend_from_slice(&[0x80, 0x00, 0x00]);
    send(&mut stream, Command::Process, order, &process);
    for command in UNHANDLED {
        send(&mut stream, command, order, &[0; 4]);
    }
    send(&mut stream, Command::Echo, order, b"cmd16");

    let mut before_echo = Vec::new();
    loop {
        let (cmd, body) = next_app_frame(&mut stream, order);
        if cmd == Command::Echo.code() {
            assert_eq!(body, b"cmd16");
            return (before_echo, me);
        }
        before_echo.push(cmd);
    }
}

#[test]
fn pvxs_ignores_a_process_request() {
    let ioc_bin = require_pvxs(SOFT_IOC_PVX);
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("t.db");
    std::fs::write(&db, format!("record(ai, \"{PV}\") {{}}\n")).expect("write db");
    let logs = PvxsLogs::in_dir(dir.path(), "ioc");
    let build = || {
        let tcp = pick_localhost_port();
        let mut cmd = soft_ioc_pvx_command(&ioc_bin, &db, tcp, named_port::free_for_tcp_and_udp());
        cmd.env("PVXS_LOG", "pvxs.tcp.io=DEBUG");
        (cmd, SocketAddr::from(([127, 0, 0, 1], tcp)))
    };
    let (_ioc, addr) = start_pvxs(&logs, build, ioc_ready).unwrap_or_else(|e| panic!("{e}"));
    let (replies, me) = replies_to_cmd16(addr);
    assert_eq!(replies, Vec::<u8>::new());
    let lines = ignore_lines(me);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let err = loop {
        let err = std::fs::read_to_string(&logs.err).unwrap_or_default();
        if lines.iter().all(|l| err.contains(l)) || std::time::Instant::now() > deadline {
            break err;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    for line in lines {
        assert!(err.contains(&line), "{line:?} not in:\n{err}");
    }
}

#[cfg(tokio_backend)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rs_ignores_a_process_request_as_pvxs_does() {
    use epics_base_rs::runtime::log;
    use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
    use epics_pva_rs::pvxs_log::{Level, TCP_IO, logger_level_set};
    use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};

    let pv = SharedPV::new();
    pv.open(
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".to_string(),
            fields: vec![("value".to_string(), FieldDesc::Scalar(ScalarType::Double))].into(),
        },
        PvField::Structure(PvStructure {
            struct_id: "epics:nt/NTScalar:1.0".to_string(),
            fields: vec![(
                "value".to_string(),
                PvField::Scalar(ScalarValue::Double(0.0)),
            )],
        }),
    )
    .unwrap();
    let source = SharedSource::new();
    source.add(PV, pv);
    let server = PvaServer::isolated(std::sync::Arc::new(source)).expect("server start");
    let addr = server.tcp_addr();

    logger_level_set(TCP_IO, Level::Debug);
    let heard = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let sink = heard.clone();
    let id = log::errlog_add_listener(move |m| sink.lock().unwrap().push_str(m));
    let (replies, me) = tokio::task::spawn_blocking(move || replies_to_cmd16(addr))
        .await
        .unwrap();
    log::errlog_flush();
    log::errlog_remove_listener(id);
    assert_eq!(replies, Vec::<u8>::new());
    let heard = heard.lock().unwrap();
    for line in ignore_lines(me) {
        assert!(heard.contains(&line), "{line:?} not in:\n{heard}");
    }
}
