//! **CA serves the record's stamp raw whatever `info(Q:time:tag)` says.**
//!
//! `Q:time:tag` is a QSRV mapping (pvxs `iocsource.cpp:239-248`); rsrv's GET
//! (`dbChannel_get_count`) and its updates (`db_post_events`) never consult
//! it, so a CA `DBR_TIME` carries the full nanoseconds and no split. softIocPVX
//! (pvxs 1.5.3, EPICS R7.0.10) on the record below, a raw CA client beside
//! `pvxget` reading the same stamp:
//!
//! ```text
//! pvxget  timeStamp.nanoseconds = 999966976  timeStamp.userTag = 106
//! GET     nsec=999967082  (nsec & 0xff = 0x6a)
//! FIRST   nsec=999967082  (nsec & 0xff = 0x6a)
//! -- caput TAG.PROC 1 --
//! pvxget  timeStamp.nanoseconds = 593824768  timeStamp.userTag = 214
//! UPDATE  nsec=593824982  (nsec & 0xff = 0xd6)
//! ```

#![cfg(tokio_backend)]

use epics_base_rs::types::WallTime;
use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_ca_rs::protocol::{
    CA_MINOR_VERSION, CA_PROTO_ACCESS_RIGHTS, CA_PROTO_CLIENT_NAME, CA_PROTO_CREATE_CHAN,
    CA_PROTO_EVENT_ADD, CA_PROTO_HOST_NAME, CA_PROTO_READ_NOTIFY, CA_PROTO_VERSION, CaHeader,
    pad_string,
};
use epics_ca_rs::server::CaServer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const DB: &str = r#"
record(ai, "TAG") {
  field(VAL, "1.5")
  field(MDEL, "-1")
  info(Q:time:tag, "nsec:lsb:8")
}
"#;

const DBR_TIME_DOUBLE: u16 = 20;
/// 1990-01-01 in Unix seconds.
const EPICS_EPOCH: u64 = 631_152_000;
/// The capture's first stamp, low byte 0x6a.
const STAMP_NSEC: u32 = 999_967_082;

fn frame(
    cmd: u16,
    data_type: u16,
    count: u32,
    cid: u32,
    available: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut h = CaHeader::new(cmd);
    h.data_type = data_type;
    h.cid = cid;
    h.available = available;
    h.set_payload_size(payload.len(), count, CA_MINOR_VERSION)
        .unwrap();
    let mut f = h.to_bytes().to_vec();
    f.extend_from_slice(payload);
    f
}

async fn next(sock: &mut TcpStream, cmd: u16) -> Vec<u8> {
    loop {
        let mut h = [0u8; 16];
        tokio::time::timeout(Duration::from_secs(5), sock.read_exact(&mut h))
            .await
            .expect("frame within 5 s")
            .unwrap();
        let mut body = vec![0u8; u16::from_be_bytes([h[2], h[3]]) as usize];
        sock.read_exact(&mut body).await.unwrap();
        if u16::from_be_bytes([h[0], h[1]]) == cmd {
            return body;
        }
    }
}

/// `dbr_time_double`'s (secPastEpoch, nsec).
fn stamp(body: &[u8]) -> (u32, u32) {
    let word = |i: usize| u32::from_be_bytes(body[i..i + 4].try_into().unwrap());
    (word(4), word(8))
}

fn record_stamp(db: &PvDatabase) -> (u32, u32) {
    let t = db.get_record("TAG").unwrap().read().common.time;
    let d = t.since_unix_epoch();
    ((d.as_secs() - EPICS_EPOCH) as u32, d.subsec_nanos())
}

/// GET, the first monitor value and an update after PROC on one circuit.
async fn three_reads() -> [((u32, u32), (u32, u32)); 3] {
    let server = CaServer::builder()
        .port(0)
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let port = server.tcp_port();
    let db = server.database().clone();
    tokio::spawn(async move { server.run().await });
    db.get_record("TAG").unwrap().write().common.time =
        WallTime::UNIX_EPOCH + Duration::new(1_700_000_000, STAMP_NSEC);

    let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    sock.write_all(&frame(
        CA_PROTO_VERSION,
        0,
        CA_MINOR_VERSION as u32,
        0,
        0,
        &[],
    ))
    .await
    .unwrap();
    sock.write_all(&frame(CA_PROTO_CLIENT_NAME, 0, 0, 0, 0, &pad_string("u")))
        .await
        .unwrap();
    sock.write_all(&frame(CA_PROTO_HOST_NAME, 0, 0, 0, 0, &pad_string("h")))
        .await
        .unwrap();
    sock.write_all(&frame(
        CA_PROTO_CREATE_CHAN,
        0,
        0,
        1,
        CA_MINOR_VERSION as u32,
        &pad_string("TAG"),
    ))
    .await
    .unwrap();
    next(&mut sock, CA_PROTO_ACCESS_RIGHTS).await;
    let mut h = [0u8; 16];
    sock.read_exact(&mut h).await.unwrap();
    assert_eq!(u16::from_be_bytes([h[0], h[1]]), CA_PROTO_CREATE_CHAN);
    let sid = u32::from_be_bytes(h[12..16].try_into().unwrap());

    sock.write_all(&frame(
        CA_PROTO_READ_NOTIFY,
        DBR_TIME_DOUBLE,
        1,
        sid,
        9,
        &[],
    ))
    .await
    .unwrap();
    let get = (
        stamp(&next(&mut sock, CA_PROTO_READ_NOTIFY).await),
        record_stamp(&db),
    );

    let mut mon = [0u8; 16];
    mon[12..14].copy_from_slice(&5u16.to_be_bytes()); // DBE_VALUE | DBE_ALARM
    sock.write_all(&frame(CA_PROTO_EVENT_ADD, DBR_TIME_DOUBLE, 1, sid, 7, &mon))
        .await
        .unwrap();
    let first = (
        stamp(&next(&mut sock, CA_PROTO_EVENT_ADD).await),
        record_stamp(&db),
    );

    // A processed stamp's low byte is 0 one time in 256, which a split would
    // also serve; process until it is not.
    let update = loop {
        db.process_record("TAG").await.unwrap();
        let update = (
            stamp(&next(&mut sock, CA_PROTO_EVENT_ADD).await),
            record_stamp(&db),
        );
        if update.1.1 & 0xff != 0 {
            break update;
        }
    };
    [get, first, update]
}

#[epics_macros_rs::epics_test]
async fn a_time_tag_get_is_the_raw_stamp() {
    let [(got, record), ..] = three_reads().await;
    assert_eq!(record.1, STAMP_NSEC);
    assert_eq!(got, record, "GET nsec (low byte {:#04x})", got.1 & 0xff);
}

#[epics_macros_rs::epics_test]
async fn a_time_tag_first_monitor_value_is_the_raw_stamp() {
    let [_, (got, record), _] = three_reads().await;
    assert_eq!(record.1, STAMP_NSEC);
    assert_eq!(
        got,
        record,
        "first value nsec (low byte {:#04x})",
        got.1 & 0xff
    );
}

#[epics_macros_rs::epics_test]
async fn a_time_tag_update_is_the_raw_stamp() {
    let [.., (got, record)] = three_reads().await;
    assert_eq!(got, record, "update nsec (low byte {:#04x})", got.1 & 0xff);
}
