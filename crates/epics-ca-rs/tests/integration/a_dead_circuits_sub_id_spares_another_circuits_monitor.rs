//! **A circuit's teardown removes its own monitors, never another circuit's
//! that happens to carry the same client-chosen sub_id.**
//!
//! Two `camonitor X` processes pick the same subscription id. Against C
//! softIoc, killing one and then `caput X 2`, `caput X 3` leaves the other
//! printing `1`, `2`, `3`: `db_cancel_event` unlinks the one `evSubscrip` it
//! was handed (`dbEvent.c:470-486`). The wire shape here is that run.
//!
//! Both receive loops are driven: each tears a circuit down through its own
//! subscription map.

use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;
use epics_ca_rs::protocol::{CA_PROTO_EVENT_ADD, CA_PROTO_WRITE_NOTIFY};
#[cfg(tokio_backend)]
use epics_ca_rs::server::CaServer;
use raw_ca::*;

const PV: &str = "SUBID:X";
/// The id both clients choose, as two libca processes do.
const SUB_ID: u32 = 1;

#[cfg(tokio_backend)]
#[test]
fn the_async_loop_spares_the_surviving_circuits_monitor() {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<(u16, Arc<PvDatabase>)>();
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
            ready_tx
                .send((server.tcp_port(), server.database().clone()))
                .expect("report tcp port");
            tokio::select! {
                _ = server.run() => {}
                _ = tokio::task::spawn_blocking(move || { let _ = stop_rx.recv(); }) => {}
            }
        });
    });

    let (port, db) = ready_rx.recv().expect("server reports its port");
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    assert_survivor_sees_every_update(addr, &db);

    let _ = stop_tx.send(());
    let _ = server_thread.join();
}

#[test]
fn the_blocking_loop_spares_the_surviving_circuits_monitor() {
    let db = Arc::new(PvDatabase::new());
    epics_base_rs::runtime::task::block_on_sync(db.add_record(PV, Box::new(AoRecord::new(1.0))))
        .expect("no async runtime on this thread")
        .expect("add_record");
    let (server, addr, accept) = start_server(db.clone());

    assert_survivor_sees_every_update(addr, &db);

    server.shutdown();
    let _ = accept.join();
}

fn monitors_on_val(db: &PvDatabase) -> usize {
    let rec = db.get_record(PV).expect("record");
    rec.read().subscriber_count("VAL")
}

fn assert_survivor_sees_every_update(addr: SocketAddr, db: &PvDatabase) {
    let mut a = connect_and_handshake(addr);
    let sid_a = create_channel(&mut a, 0xA, PV);
    a.send(&event_add_frame(sid_a, SUB_ID));
    assert_eq!(
        payload_double(&a.expect(CA_PROTO_EVENT_ADD, "A's initial")),
        1.0
    );

    let mut b = connect_and_handshake(addr);
    let sid_b = create_channel(&mut b, 0xB, PV);
    b.send(&event_add_frame(sid_b, SUB_ID));
    assert_eq!(
        payload_double(&b.expect(CA_PROTO_EVENT_ADD, "B's initial")),
        1.0
    );
    assert_eq!(monitors_on_val(db), 2);

    // B's circuit dies. Wait for its teardown to have run, so the puts below
    // are ordered after it.
    drop(b);
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while monitors_on_val(db) == 2 {
        assert!(Instant::now() < deadline, "B's circuit was never torn down");
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(monitors_on_val(db), 1, "only B's monitor left the record");

    for (ioid, v) in [(2, 2.0), (3, 3.0)] {
        a.send(&write_frame(CA_PROTO_WRITE_NOTIFY, sid_a, ioid, v));
        a.expect(CA_PROTO_WRITE_NOTIFY, "A's put callback");
        assert_eq!(
            payload_double(&a.expect(CA_PROTO_EVENT_ADD, "A's update")),
            v,
            "softIoc delivers every later update to the surviving monitor"
        );
    }
}
