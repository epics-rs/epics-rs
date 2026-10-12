//! **The initial value of a monitor added under EVENTS_OFF waits for
//! EVENTS_ON.**
//!
//! `event_add_action` queues the initial value through `db_post_single_event`
//! (`camessage.c:1904`) onto the circuit's event queue, which EVENTS_OFF
//! holds. Against C softIoc a raw client sending EVENTS_OFF, EVENT_ADD and a
//! READ_NOTIFY sees the read reply and no EVENT_ADD frame; after EVENTS_ON it
//! sees exactly one, carrying the value at the add. The wire shape here is
//! that run.

// RTEMS-EXEC-MODEL-ALLOW(1): measured, not argued — the site is the async
// loop's runtime, which its `cfg(tokio_backend)` test keeps off the exec
// backend; the blocking loop's case runs and passes under
// `EPICS_RS_BUILD_EXEC_BACKEND=thread`.

use std::net::SocketAddr;
use std::sync::Arc;
#[cfg(tokio_backend)]
use std::thread;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;
use epics_ca_rs::protocol::{
    CA_PROTO_EVENT_ADD, CA_PROTO_EVENTS_OFF, CA_PROTO_EVENTS_ON, CA_PROTO_READ_NOTIFY,
    CA_PROTO_WRITE_NOTIFY, CaHeader,
};
#[cfg(tokio_backend)]
use epics_ca_rs::server::CaServer;
use raw_ca::*;

const PV: &str = "EVOFF:X";

#[cfg(tokio_backend)]
#[test]
fn the_async_loop_holds_the_initial_value_until_events_on() {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<u16>();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let server_thread = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async move {
            let server = CaServer::builder()
                .port(0)
                .tcp_port(0)
                .record(PV, AoRecord::new(1.0))
                .build()
                .await
                .expect("build CA server");
            ready_tx.send(server.tcp_port()).expect("report tcp port");
            tokio::select! {
                _ = server.run() => {}
                _ = tokio::task::spawn_blocking(move || { let _ = stop_rx.recv(); }) => {}
            }
        });
    });

    let port = ready_rx.recv().expect("server reports its port");
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    assert_initial_value_waits_for_events_on(addr);

    let _ = stop_tx.send(());
    let _ = server_thread.join();
}

#[test]
fn the_blocking_loop_holds_the_initial_value_until_events_on() {
    let db = Arc::new(PvDatabase::new());
    epics_base_rs::runtime::task::block_on_sync(db.add_record(PV, Box::new(AoRecord::new(1.0))))
        .expect("no async runtime on this thread")
        .expect("add_record");
    let (server, addr, accept) = start_server(db);

    assert_initial_value_waits_for_events_on(addr);

    server.shutdown();
    let _ = accept.join();
}

fn bare(cmmd: u16) -> Vec<u8> {
    CaHeader::new(cmmd).to_bytes().to_vec()
}

fn assert_initial_value_waits_for_events_on(addr: SocketAddr) {
    let mut c = connect_and_handshake(addr);
    let sid = create_channel(&mut c, 0xE, PV);

    c.send(&bare(CA_PROTO_EVENTS_OFF));
    c.send(&event_add_frame(sid, 1));
    c.send(&read_notify_frame(sid, 7));
    c.expect_absent_before(
        |f| cmmd_of(f) == CA_PROTO_EVENT_ADD,
        |f| cmmd_of(f) == CA_PROTO_READ_NOTIFY,
        "softIoc sends no EVENT_ADD frame while EVENTS_OFF holds the queue",
    );

    c.send(&bare(CA_PROTO_EVENTS_ON));
    assert_eq!(
        payload_double(&c.expect(CA_PROTO_EVENT_ADD, "the initial value")),
        1.0
    );
    c.send(&write_frame(CA_PROTO_WRITE_NOTIFY, sid, 8, 2.0));
    c.expect(CA_PROTO_WRITE_NOTIFY, "put callback");
    assert_eq!(
        payload_double(&c.expect(CA_PROTO_EVENT_ADD, "the next frame")),
        2.0,
        "softIoc sends the initial value once"
    );
}
