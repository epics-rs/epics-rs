//! An ARRAY (command 14) request on a live channel, against pvxs 1.5.3
//! softIocPVX and against the rs server.
//!
//! `ServerConn` has no ARRAY handler, so `ConnBase::bevRead`'s default arm
//! (`conn.cpp:248-253`) logs "Ignore unexpected command 0x0e", drops the
//! segment and sends nothing; the ECHO sent after it is the next reply.

use std::io::Write as _;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use epics_pva_rs::proto::{Command, ReadExt, WriteExt, encode_string_into};

use super::interop_helpers::{
    PvxsLogs, SOFT_IOC_PVX, ioc_ready, pick_localhost_port, require_pvxs, soft_ioc_pvx_command,
    start_pvxs,
};
use super::process_cmd16::{handshake, next_app_frame, send};

const PV: &str = "CMD14:AI";

/// `conn.cpp:250`'s line for `command` from a client at `peer`.
pub(super) fn ignore_line(peer: SocketAddr, command: Command) -> String {
    format!(
        "DEBUG pvxs.tcp.io Client {peer} Ignore unexpected command 0x{:02x}",
        command.code()
    )
}

/// The application replies to an INIT of each of `commands` on a connected
/// channel that arrive before the answer to an ECHO sent after them, and the
/// client's own address.
fn replies_to_inits(addr: SocketAddr, commands: &[Command]) -> (Vec<u8>, SocketAddr) {
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
    for (ioid, &command) in (7..).zip(commands) {
        let mut init = Vec::new();
        init.put_u32(sid, order);
        init.put_u32(ioid, order);
        init.put_u8(0x08);
        init.extend_from_slice(&[0x80, 0x00, 0x00]);
        send(&mut stream, command, order, &init);
    }
    send(&mut stream, Command::Echo, order, b"cmd14");
    stream.flush().expect("flush");

    let mut before_echo = Vec::new();
    loop {
        let (cmd, body) = next_app_frame(&mut stream, order);
        if cmd == Command::Echo.code() {
            assert_eq!(body, b"cmd14");
            return (before_echo, me);
        }
        before_echo.push(cmd);
    }
}

/// `replies_to_inits` against softIocPVX, and its stderr once the ARRAY
/// ignore line is in it: pvxs reads a circuit in order, so by then every
/// command sent before the ARRAY has been handled.
pub(super) fn on_pvxs(commands: &[Command]) -> (Vec<u8>, SocketAddr, String) {
    let ioc_bin = require_pvxs(SOFT_IOC_PVX);
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("t.db");
    std::fs::write(
        &db,
        format!("record(waveform, \"{PV}\") {{ field(FTVL, \"DOUBLE\") field(NELM, \"4\") }}\n"),
    )
    .expect("write db");
    let logs = PvxsLogs::in_dir(dir.path(), "ioc");
    let build = || {
        let tcp = pick_localhost_port();
        let mut cmd = soft_ioc_pvx_command(&ioc_bin, &db, tcp, named_port::free_for_tcp_and_udp());
        cmd.env("PVXS_LOG", "pvxs.tcp.io=DEBUG");
        (cmd, SocketAddr::from(([127, 0, 0, 1], tcp)))
    };
    let (_ioc, addr) = start_pvxs(&logs, build, ioc_ready).unwrap_or_else(|e| panic!("{e}"));
    let (replies, me) = replies_to_inits(addr, commands);
    let line = ignore_line(me, Command::Array);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let err = loop {
        let err = std::fs::read_to_string(&logs.err).unwrap_or_default();
        if err.contains(&line) || std::time::Instant::now() > deadline {
            break err;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    (replies, me, err)
}

/// `replies_to_inits` against the rs server, and what it logged.
#[cfg(tokio_backend)]
pub(super) async fn on_rs(commands: &'static [Command]) -> (Vec<u8>, SocketAddr, String) {
    use epics_base_rs::runtime::log;
    use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
    use epics_pva_rs::pvxs_log::{Level, TCP_IO, logger_level_set};
    use epics_pva_rs::server_native::{PvaServer, SharedPV, SharedSource};

    let pv = SharedPV::new();
    pv.open(
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalarArray:1.0".to_string(),
            fields: vec![(
                "value".to_string(),
                FieldDesc::ScalarArray(ScalarType::Double),
            )]
            .into(),
        },
        PvField::Structure(PvStructure {
            struct_id: "epics:nt/NTScalarArray:1.0".to_string(),
            fields: vec![(
                "value".to_string(),
                PvField::ScalarArray(vec![ScalarValue::Double(1.0), ScalarValue::Double(2.0)]),
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
    let (replies, me) = tokio::task::spawn_blocking(move || replies_to_inits(addr, commands))
        .await
        .unwrap();
    log::errlog_flush();
    log::errlog_remove_listener(id);
    let heard = heard.lock().unwrap().clone();
    (replies, me, heard)
}

#[test]
fn pvxs_ignores_an_array_request() {
    let (replies, me, err) = on_pvxs(&[Command::Array]);
    assert_eq!(replies, Vec::<u8>::new());
    let line = ignore_line(me, Command::Array);
    assert!(err.contains(&line), "{line:?} not in:\n{err}");
}

#[cfg(tokio_backend)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rs_ignores_an_array_request_as_pvxs_does() {
    let (replies, me, heard) = on_rs(&[Command::Array]).await;
    assert_eq!(replies, Vec::<u8>::new());
    let line = ignore_line(me, Command::Array);
    assert!(heard.contains(&line), "{line:?} not in:\n{heard}");
}
