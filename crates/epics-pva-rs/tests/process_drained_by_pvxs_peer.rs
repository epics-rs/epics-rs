//! CMD_PROCESS against a peer that does not implement it (R5-PVA-1).
//!
//! pvxs has no PROCESS handler. The constant exists once, as
//! `CMD_PROCESS = 16` (`pvxs/src/pvaproto.h:632`), and `ConnBase`'s command
//! switch (`pvxs/src/conn.cpp:249-276`) lists every command it handles —
//! ECHO, CONNECTION_VALIDATION/VALIDATED, SEARCH, SEARCH_RESPONSE, AUTHNZ,
//! CREATE_CHANNEL, DESTROY_CHANNEL, GET, PUT, PUT_GET, MONITOR, RPC,
//! CANCEL_REQUEST, DESTROY_REQUEST, GET_FIELD, MESSAGE — and cmd 16 is not
//! among them. It therefore falls to `default:`, which
//!
//! ```text
//! log_debug_printf(connio, "%s %s Ignore unexpected command 0x%02x\n", …);
//! evbuffer_drain(segBuf.get(), evbuffer_get_length(segBuf.get()));
//! break;
//! ```
//!
//! — logs, discards the body, and replies nothing. The circuit stays up and
//! the operation simply never answers.
//!
//! The PVA-to-PVA gateway relays a downstream PROCESS upstream verbatim
//! (`epics-bridge-rs/src/pva_gateway/source.rs:1543-1546`), exactly as
//! pva2pva's `GWChannel::createChannelProcess` forwards to
//! `entry->channel->createChannelProcess(requester, pvRequest)`
//! (`pva2pva/p2pApp/channel.cpp:98-107`). So a gateway in front of a pvxs IOC
//! meets this silence, and what matters is that the client leg bounds it: the
//! op must fail on the op timeout rather than hang, and the circuit must
//! survive so the other channels on it keep working.
//!
//! The scripted server below is that `default:` arm.

// RTEMS-EXEC-MODEL-ALLOW(1): checked - these run and pass in the exec-backend
// suite.
#![cfg(test)]
#![cfg(feature = "client")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::proto::{
    Command, ControlCommand, PvaHeader, Status, WriteExt, encode_size_into, encode_string_into,
};

const ORDER: epics_pva_rs::proto::ByteOrder = epics_pva_rs::proto::ByteOrder::Little;
const SID: u32 = 42;

fn app_frame(cmd: u8, payload: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    PvaHeader::application(true, ORDER, cmd, payload.len() as u32).write_into(&mut out);
    out.extend_from_slice(&payload);
    out
}

/// Counters the test reads back out of the scripted peer.
#[derive(Default)]
struct PeerLog {
    /// PROCESS frames that reached the `default:` arm and were drained.
    drained: AtomicUsize,
    /// Set when the peer's read loop ends — a closed circuit.
    closed: AtomicBool,
}

/// A pvxs-faithful peer whose command switch has no `CASE(PROCESS)`.
///
/// Handshake and CREATE_CHANNEL are answered (pvxs implements both); every
/// other command is counted and discarded with no reply, and the connection
/// is left open — `evbuffer_drain` + `break`, not `bev.reset()`.
async fn spawn_peer_without_process(log: Arc<PeerLog>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind scripted peer");
    let addr = listener.local_addr().expect("scripted peer addr");

    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            tokio::spawn(serve_conn(sock, log.clone()));
        }
    });

    addr
}

async fn serve_conn(mut sock: tokio::net::TcpStream, log: Arc<PeerLog>) {
    // SET_BYTE_ORDER (control) + CONNECTION_VALIDATION request.
    let mut hello = Vec::new();
    PvaHeader::control(true, ORDER, ControlCommand::SetByteOrder.code(), 0).write_into(&mut hello);
    let mut p = Vec::new();
    p.put_u32(0x10000, ORDER); // buffer size
    p.put_u16(32_767, ORDER); // registry size
    encode_size_into(1, ORDER, &mut p); // one auth method
    encode_string_into("anonymous", ORDER, &mut p);
    hello.extend_from_slice(&app_frame(Command::ConnectionValidation.code(), p));
    if sock.write_all(&hello).await.is_err() {
        log.closed.store(true, Ordering::SeqCst);
        return;
    }

    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let Ok(n) = sock.read(&mut chunk).await else {
            log.closed.store(true, Ordering::SeqCst);
            return;
        };
        if n == 0 {
            log.closed.store(true, Ordering::SeqCst);
            return;
        }
        buf.extend_from_slice(&chunk[..n]);

        while buf.len() >= 8 {
            let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
            if buf.len() < 8 + len {
                break;
            }
            let cmd = buf[3];
            let payload = buf[8..8 + len].to_vec();
            buf.drain(..8 + len);

            if cmd == Command::ConnectionValidation.code() {
                let out = app_frame(Command::ConnectionValidated.code(), vec![0xFF]);
                if sock.write_all(&out).await.is_err() {
                    log.closed.store(true, Ordering::SeqCst);
                    return;
                }
            } else if cmd == Command::CreateChannel.code() {
                // count(u16) + cid(u32) + name.
                let cid = u32::from_le_bytes([payload[2], payload[3], payload[4], payload[5]]);
                let mut p = Vec::new();
                p.put_u32(cid, ORDER);
                p.put_u32(SID, ORDER);
                Status::ok().write_into(ORDER, &mut p);
                if sock
                    .write_all(&app_frame(Command::CreateChannel.code(), p))
                    .await
                    .is_err()
                {
                    log.closed.store(true, Ordering::SeqCst);
                    return;
                }
            } else {
                // `default:` — log, drain, reply nothing, keep the circuit.
                if cmd == Command::Process.code() {
                    log.drained.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    }
}

/// A PROCESS to a peer with no PROCESS handler fails on the op timeout, and
/// the circuit survives.
///
/// This is what a gateway relay meets when its upstream is pvxs. The failure
/// is bounded and reported: `Context::pvprocess` passes the client timeout
/// into `op_process` and `op_process_with_request_attempt` awaits the INIT
/// reply under it, so silence becomes an error, not a stalled downstream
/// operation. `upstream_op_error` then turns it into a downstream
/// `OpError::failed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_against_a_peer_without_a_process_handler_fails_on_the_op_timeout() {
    const OP_TIMEOUT: Duration = Duration::from_millis(600);

    let log = Arc::new(PeerLog::default());
    let addr = spawn_peer_without_process(log.clone()).await;
    let client = PvaClient::builder()
        .timeout(OP_TIMEOUT)
        .server_addr(addr)
        .build();

    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(10), client.pvprocess("dut"))
        .await
        .expect("PROCESS hung past 10s — the op timeout did not bound the peer's silence");
    let elapsed = started.elapsed();

    let err = result.expect_err("a peer that drains CMD_PROCESS must not report success");

    assert_eq!(
        log.drained.load(Ordering::SeqCst),
        1,
        "the peer must have seen exactly one CMD_PROCESS frame and drained it, \
         got {} (error was {err})",
        log.drained.load(Ordering::SeqCst),
    );
    assert!(
        !log.closed.load(Ordering::SeqCst),
        "pvxs `default:` drains and breaks; the circuit must still be open \
         after the failed PROCESS (error was {err})"
    );
    assert!(
        elapsed < OP_TIMEOUT * 4,
        "the PROCESS took {elapsed:?} against a {OP_TIMEOUT:?} op timeout — \
         the wait is not bounded by the timeout (error was {err})"
    );
}
