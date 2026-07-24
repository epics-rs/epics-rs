//! A client GET must not pipeline INIT and EXEC into one write (R18-24).
//!
//! A pvxs server dispatches every complete message already in its receive
//! buffer in ONE pass — `ConnBase::bevRead()` is `while(bev && remaining >= 8)`
//! (`conn.cpp:152-153`) — so two frames that share a TCP segment are handled
//! back-to-back with nothing running in between. Its GET/PUT/RPC handler
//! answers an EXEC that arrives while the operation is still
//! `ServerOp::Creating` by killing the circuit:
//!
//! ```text
//! } else if(!(op=std::dynamic_pointer_cast<ServerGPR>(it->second)) || op->state==ServerOp::Creating) {
//!     log_err_printf(connio, "Client %s Gets invalid IOID %u state=%d\n", …);
//!     bev.reset();
//!     return;
//! }
//! ```
//! (`serverget.cpp:429-434`)
//!
//! The op leaves `Creating` only when the Source answers the INIT. A Source
//! that connects inline wins the race; one that does not — a gateway, an
//! un-`open()`ed `SharedPV` (`sharedpv.cpp:243`) — loses it, and the whole TCP
//! circuit dies with every channel on it, with no MESSAGE and no Status.
//!
//! The scripted server below IS that state machine: it drains its read buffer
//! frame by frame, answers a GET INIT only after the drain (an asynchronous
//! Source), and resets the connection on any EXEC seen while `Creating`.
//! Pre-fix the client wrote INIT+EXEC as one buffer and the GET died on a
//! closed circuit; post-fix the EXEC is sent only after the INIT reply lands.

#![cfg(test)]

// RTEMS-EXEC-MODEL-ALLOW(1): checked - these run and pass in the feature-ON suite.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::proto::{
    Command, ControlCommand, PvaHeader, Status, WriteExt, encode_size_into, encode_string_into,
};
use epics_pva_rs::pvdata::{FieldDesc, PvField, ScalarType, ScalarValue};

const ORDER: epics_pva_rs::proto::ByteOrder = epics_pva_rs::proto::ByteOrder::Little;
const SID: u32 = 42;

/// `NTScalar<double>` with a single `value` leaf.
fn intro() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
    }
}

fn app_frame(cmd: u8, payload: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    PvaHeader::application(true, ORDER, cmd, payload.len() as u32).write_into(&mut out);
    out.extend_from_slice(&payload);
    out
}

/// A scripted pvxs-faithful server: handshake → CREATE_CHANNEL → GET.
///
/// Its GET operation has pvxs's two states. A GET INIT puts it in `Creating`
/// and queues the INIT reply to be written only *after* the current read
/// buffer has been fully drained — an asynchronous Source. A GET EXEC seen
/// while `Creating` closes the socket, pvxs's `bev.reset()`.
///
/// `reset` records that kill so the test can name it; the client observes it
/// as a dead circuit.
async fn spawn_gpr_state_machine_server(reset: Arc<AtomicBool>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind scripted server");
    let addr = listener.local_addr().expect("scripted server addr");

    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            tokio::spawn(serve_conn(sock, reset.clone()));
        }
    });

    addr
}

async fn serve_conn(mut sock: tokio::net::TcpStream, reset: Arc<AtomicBool>) {
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
        return;
    }

    // pvxs `ServerOp::state`: the op is `Creating` from the INIT until its
    // Source answers. There is no third state this test needs.
    let mut creating = false;
    let mut init_reply_due: Option<u32> = None;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let Ok(n) = sock.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);

        // `ConnBase::bevRead`: drain every complete message now in the buffer,
        // in one pass, before anything else runs.
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
                    return;
                }
            } else if cmd == Command::Get.code() {
                // sid(u32) + ioid(u32) + subcmd(u8) + …
                let ioid = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
                let subcmd = payload[8];
                if subcmd & 0x08 != 0 {
                    // INIT: the op enters `Creating`. Its Source is
                    // asynchronous, so the reply is not written from inside
                    // this dispatch pass.
                    creating = true;
                    init_reply_due = Some(ioid);
                } else if creating {
                    // EXEC while `Creating` — serverget.cpp:429-434.
                    reset.store(true, Ordering::SeqCst);
                    return; // bev.reset()
                } else {
                    // EXEC on a live op: status + changed bitset + value.
                    let mut p = Vec::new();
                    p.put_u32(ioid, ORDER);
                    p.put_u8(0x00);
                    Status::ok().write_into(ORDER, &mut p);
                    p.put_u8(0x01); // bitset: one byte follows
                    p.put_u8(0x02); // bit 1 = `value`
                    p.extend_from_slice(&1.0f64.to_le_bytes());
                    if sock
                        .write_all(&app_frame(Command::Get.code(), p))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }

        // The drain is over. Now the Source answers, and the op leaves
        // `Creating` — a pipelined EXEC has already been dispatched (and has
        // already killed the circuit) by the time we get here.
        if let Some(ioid) = init_reply_due.take() {
            let mut p = Vec::new();
            p.put_u32(ioid, ORDER);
            p.put_u8(0x08);
            Status::ok().write_into(ORDER, &mut p);
            epics_pva_rs::pvdata::encode::encode_type_desc(&intro(), ORDER, &mut p);
            if sock
                .write_all(&app_frame(Command::Get.code(), p))
                .await
                .is_err()
            {
                return;
            }
            creating = false;
        }
    }
}

/// The GET completes against a server whose Source connects asynchronously.
///
/// Pre-fix (`build_get_init` + `build_get` in one `send_for_channel_sync`) the
/// EXEC was dispatched in the same pass as the INIT, while the op was still
/// `Creating`, and the server dropped the circuit: the GET fails and every
/// other channel on that connection goes with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_sends_exec_only_after_the_init_reply() {
    let reset = Arc::new(AtomicBool::new(false));
    let addr = spawn_gpr_state_machine_server(reset.clone()).await;
    let client = PvaClient::builder().build();

    let got = tokio::time::timeout(Duration::from_secs(5), client.pvget_from("ASYNC:PV", addr))
        .await
        .expect("the GET timed out");

    assert!(
        !reset.load(Ordering::SeqCst),
        "the client pipelined GET EXEC with GET INIT: the server was still \
         ServerOp::Creating and reset the circuit (serverget.cpp:429-434)"
    );

    let value = got.expect("the GET must succeed against an asynchronous Source");
    match value {
        PvField::Structure(s) => assert_eq!(
            s.get_field("value"),
            Some(&PvField::Scalar(ScalarValue::Double(1.0))),
        ),
        other => panic!("expected an NTScalar, got {other:?}"),
    }
}
