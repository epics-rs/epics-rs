//! The blocking CA front-end serving a **real record database** — the exact
//! `realtime-ca-ioc` runtime shape — to a raw-socket client.
//!
//! `blocking_raw_client_e2e.rs` proves the wire protocol against `SimplePv`
//! fixtures. This file replaces those with genuine records loaded through
//! [`IocBuilder`] (`dbLoadRecords` + `iocInit`), which is what
//! `crates/epics-ca-rs/src/bin/realtime-ca-ioc.rs` actually runs: `ao`,
//! `longout` and `stringout` from the binary's built-in `DEMO_DB`, plus an
//! async `calcout`. Record instances carry per-type native DBR types, real
//! field tables (`EGU`, `PREC`) and real processing on put — none of which a
//! uniform `SimplePv` can exercise.
//!
//! **Feature-ON only, and here is why.** A `calcout` with `ODLY > 0` defers
//! its output through `PvDatabase::schedule_delayed_reprocess`, which is a
//! `runtime::task::spawn` of a future that `runtime::task::sleep`s. With
//! `rtems-exec-model` off, that seam routes to tokio and needs a reactor that
//! a plain `#[test]` thread does not have; with it on, the seam is the
//! std-thread background executor, which is the point. Every test here shares
//! the one `background_init()` + `IocBuilder` + `BlockingCaServer` fixture, so
//! the whole file is gated rather than split. The feature-OFF suite count is
//! therefore unchanged by this file.
//!
//! Overlap is deliberate and bounded: `async_write_notify_rtems_exec.rs`
//! already proves an ODLY completion *runs on the executor* (it captures the
//! completing thread). What is new here is the property that matters to a
//! server: while that put-callback is pending, the blocking driver's message
//! thread stays live and answers other requests on the same circuit — C
//! `camsgtask` never blocks on `dbProcessNotify`.
//!
//! Waits go through [`Circuit`], which never discards a frame. That matters
//! more here than anywhere: this is the path measured at 10/25 WRITE_NOTIFY
//! first versus 15/25 monitor-update first, so any wait that destroyed the
//! reply it was not looking for would fail roughly half the time.
//!
//! No `CaClient`, no tokio runtime, no `.await` in any test. Ephemeral ports
//! only — never 5064, per the `build() ⟹ listening` rule.

#![cfg(feature = "rtems-exec-model")]

#[path = "common/raw_ca.rs"]
mod raw_ca;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::{background_init, block_on_sync};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_ca_rs::protocol::{
    CA_PROTO_ACCESS_RIGHTS, CA_PROTO_CLEAR_CHANNEL, CA_PROTO_CREATE_CHAN, CA_PROTO_EVENT_ADD,
    CA_PROTO_READ_NOTIFY, CA_PROTO_WRITE, CA_PROTO_WRITE_NOTIFY,
};
use raw_ca::*;

/// DBR_LONG / DBR_STRING — the native wire types `longout` and `stringout`
/// announce, which is what distinguishes real records from a `SimplePv`.
const DBR_STRING: u16 = 0;
const DBR_LONG: u16 = 5;

/// The `realtime-ca-ioc` built-in `DEMO_DB`, plus an async `calcout`.
///
/// `CALC "A+1"` with `A` defaulting to 0 makes every cycle compute; `OOPT`
/// defaults to "Every Time" so `should_output()` is always true; `ODLY` then
/// defers the output by 150 ms, which is what puts the record on the
/// async-pending fork.
const DEMO_DB: &str = concat!(
    "record(ao, \"RT:AO\") { field(VAL, \"1.5\") field(PREC, \"3\") field(EGU, \"V\") }\n",
    "record(longout, \"RT:LO\") { field(VAL, \"7\") field(EGU, \"counts\") }\n",
    "record(stringout, \"RT:MSG\") { field(VAL, \"realtime-ca-ioc\") }\n",
    "record(calcout, \"RT:DLY\") { field(CALC, \"A+1\") field(ODLY, \"0.15\") }\n",
);

/// The ODLY of `RT:DLY` above — the floor the deferred put-callback must clear.
const ODLY: Duration = Duration::from_millis(150);

/// Load the real database exactly as `realtime-ca-ioc::load_database` does:
/// `background_init()` first (C `callbackInit`, so a record can defer a tail
/// before any client connects), then `IocBuilder::build` driven by
/// `block_on_sync`, which parks this thread between polls — the build future
/// awaits only in-process locks, so no reactor is involved.
fn build_real_db() -> Arc<PvDatabase> {
    background_init();
    let (db, _autosave) = block_on_sync(
        IocBuilder::new()
            .db_string(DEMO_DB, &HashMap::new())
            .expect("load db string")
            .build(),
    )
    .expect("no async runtime entered on this test thread")
    .expect("iocInit");
    db
}

/// CREATE_CHAN that also returns what the channel announced: `(sid, native
/// dbr type, element count)`. Real records answer their own native type here;
/// a `SimplePv` cannot.
fn create_channel_typed(c: &mut Circuit, cid: u32, pv: &str) -> (u32, u16, u32) {
    c.send(&create_chan_frame(cid, pv));
    // ACCESS_RIGHTS accompanies the reply; claim it so it is not left queued.
    let replies = c.expect_all(
        &[CA_PROTO_ACCESS_RIGHTS, CA_PROTO_CREATE_CHAN],
        "CREATE_CHAN and its ACCESS_RIGHTS",
    );
    let cc = &replies[1];
    // CREATE_CHAN reply layout: data_type at 4..6, data_count at 6..8,
    // our cid at 8..12, the server's sid at 12..16.
    let native = u16::from_be_bytes([cc[4], cc[5]]);
    let count = u32::from(u16::from_be_bytes([cc[6], cc[7]]));
    assert_eq!(
        u32::from_be_bytes([cc[8], cc[9], cc[10], cc[11]]),
        cid,
        "CREATE_CHAN reply echoes our cid"
    );
    let sid = u32::from_be_bytes([cc[12], cc[13], cc[14], cc[15]]);
    (sid, native, count)
}

/// A DBR_LONG scalar in a reply payload.
fn payload_long(frame: &[u8]) -> i32 {
    i32::from_be_bytes([frame[16], frame[17], frame[18], frame[19]])
}

/// The NUL-terminated DBR_STRING in a reply payload.
fn payload_string(frame: &[u8]) -> String {
    let body = &frame[16..];
    let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
    String::from_utf8_lossy(&body[..end]).into_owned()
}

/// The ioid a reply echoes, at bytes 12..16.
fn ioid_of(frame: &[u8]) -> u32 {
    u32::from_be_bytes([frame[12], frame[13], frame[14], frame[15]])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Real records announce their own native DBR type and element count on
/// CREATE_CHAN, and serve their seeded values over READ_NOTIFY. This is the
/// per-record typing a `SimplePv` fixture cannot show.
#[test]
fn real_records_announce_native_types_and_serve_their_values() {
    let (server, addr, accept) = start_server(build_real_db());
    let mut c = connect_and_handshake(addr);

    let (ao_sid, ao_native, ao_count) = create_channel_typed(&mut c, 1, "RT:AO");
    assert_eq!(ao_native, DBR_DOUBLE, "ao is natively DBR_DOUBLE");
    assert_eq!(ao_count, 1, "ao is a scalar");

    let (lo_sid, lo_native, lo_count) = create_channel_typed(&mut c, 2, "RT:LO");
    assert_eq!(lo_native, DBR_LONG, "longout is natively DBR_LONG");
    assert_eq!(lo_count, 1, "longout is a scalar");

    let (msg_sid, msg_native, _) = create_channel_typed(&mut c, 3, "RT:MSG");
    assert_eq!(msg_native, DBR_STRING, "stringout is natively DBR_STRING");

    // READ_NOTIFY each in its own native type.
    c.send(&read_notify_frame(ao_sid, 0x01));
    let r = c.expect(CA_PROTO_READ_NOTIFY, "ao READ_NOTIFY");
    assert_eq!(payload_double(&r), 1.5, "ao VAL from the db string");

    let mut lo_req = read_notify_frame(lo_sid, 0x02);
    lo_req[4..6].copy_from_slice(&DBR_LONG.to_be_bytes());
    c.send(&lo_req);
    let r = c.expect(CA_PROTO_READ_NOTIFY, "longout READ_NOTIFY");
    assert_eq!(payload_long(&r), 7, "longout VAL from the db string");

    let mut msg_req = read_notify_frame(msg_sid, 0x03);
    msg_req[4..6].copy_from_slice(&DBR_STRING.to_be_bytes());
    c.send(&msg_req);
    let r = c.expect(CA_PROTO_READ_NOTIFY, "stringout READ_NOTIFY");
    assert_eq!(
        payload_string(&r),
        "realtime-ca-ioc",
        "stringout VAL from the db string"
    );

    drop(c);
    server.shutdown();
    accept.join().unwrap();
}

/// A record's *field* is addressable as its own channel — `RT:AO.EGU` and
/// `RT:AO.PREC` come from the real `ao` field table, which is exactly what a
/// synthetic PV has none of.
#[test]
fn record_fields_are_addressable_as_channels() {
    let (server, addr, accept) = start_server(build_real_db());
    let mut c = connect_and_handshake(addr);

    let (egu_sid, egu_native, _) = create_channel_typed(&mut c, 1, "RT:AO.EGU");
    assert_eq!(egu_native, DBR_STRING, "EGU is a string field");
    let mut req = read_notify_frame(egu_sid, 1);
    req[4..6].copy_from_slice(&DBR_STRING.to_be_bytes());
    c.send(&req);
    let r = c.expect(CA_PROTO_READ_NOTIFY, "EGU READ_NOTIFY");
    assert_eq!(payload_string(&r), "V", "ao EGU from the db string");

    let (prec_sid, _, _) = create_channel_typed(&mut c, 2, "RT:AO.PREC");
    c.send(&read_notify_frame(prec_sid, 2));
    let r = c.expect(CA_PROTO_READ_NOTIFY, "PREC READ_NOTIFY");
    assert_eq!(payload_double(&r), 3.0, "ao PREC from the db string");

    drop(c);
    server.shutdown();
    accept.join().unwrap();
}

/// Writes, monitors and teardown against a real `ao`: WRITE_NOTIFY lands and
/// replies, the subscription sees the change, EVENT_CANCEL stops delivery and
/// CLEAR_CHANNEL closes the channel.
#[test]
fn writes_and_monitors_work_against_a_real_ao_record() {
    let (server, addr, accept) = start_server(build_real_db());
    let mut c = connect_and_handshake(addr);
    let sid = create_channel(&mut c, 0x1234, "RT:AO");

    // Subscribe: initial snapshot is the seeded VAL.
    let sub_id = 0xAB;
    c.send(&event_add_frame(sid, sub_id));
    let initial = c.expect(CA_PROTO_EVENT_ADD, "EVENT_ADD initial");
    assert_eq!(payload_double(&initial), 1.5, "initial monitor is ao VAL");

    // One write, two replies whose order this path genuinely varies (10/25
    // versus 15/25 when measured), so claim both rather than waiting on one.
    c.send(&write_frame(CA_PROTO_WRITE_NOTIFY, sid, 0x11, 6.25));
    let pair = c.expect_all(
        &[CA_PROTO_WRITE_NOTIFY, CA_PROTO_EVENT_ADD],
        "ao WRITE_NOTIFY and the monitor update it fans out",
    );
    assert_eq!(
        ioid_of(&pair[0]),
        0x11,
        "WRITE_NOTIFY reply echoes our ioid"
    );
    assert_eq!(
        payload_double(&pair[1]),
        6.25,
        "the write fans out to the subscription"
    );

    // Cancel, then prove the subscription is really gone.
    c.send(&event_cancel_frame(sid, sub_id));
    let _ = c.expect_cancel_ack(sub_id);
    c.send(&write_frame(CA_PROTO_WRITE, sid, 0, 77.0));
    c.expect_silence(
        Duration::from_millis(250),
        "a cancelled subscription on a real record must deliver nothing",
    );

    // The fire-and-forget write still landed.
    c.send(&read_notify_frame(sid, 0x12));
    let r = c.expect(CA_PROTO_READ_NOTIFY, "READ_NOTIFY after WRITE");
    assert_eq!(
        payload_double(&r),
        77.0,
        "fire-and-forget WRITE took effect"
    );

    c.send(&clear_channel_frame(sid, 0x1234));
    let cl = c.expect(CA_PROTO_CLEAR_CHANNEL, "CLEAR_CHANNEL");
    assert_eq!(
        u32::from_be_bytes([cl[8], cl[9], cl[10], cl[11]]),
        sid,
        "CLEAR_CHANNEL reply echoes the sid"
    );

    drop(c);
    server.shutdown();
    accept.join().unwrap();
}

/// The async fork, end to end over the wire: a WRITE_NOTIFY that puts the
/// `calcout` on its ODLY delay is answered only after the background executor
/// runs the deferred re-process — **and, while it is pending, the blocking
/// driver's message thread keeps answering other requests on the same
/// circuit** (C `camsgtask` never blocks on `dbProcessNotify`).
///
/// That interleaving is the property this test exists for; that the completion
/// runs on the executor at all is `async_write_notify_rtems_exec.rs`'s job.
#[test]
fn a_pending_put_callback_does_not_block_the_circuit() {
    let (server, addr, accept) = start_server(build_real_db());
    let mut c = connect_and_handshake(addr);

    // Processing the calcout is triggered by a put to its PROC field.
    let proc_sid = create_channel(&mut c, 1, "RT:DLY.PROC");
    let ao_sid = create_channel(&mut c, 2, "RT:AO");

    let started = Instant::now();
    c.send(&write_frame(CA_PROTO_WRITE_NOTIFY, proc_sid, 0x21, 1.0));

    // While the ODLY delay is running, an unrelated READ_NOTIFY on the same
    // circuit must be answered — the message thread is not parked on the
    // put-callback.
    c.send(&read_notify_frame(ao_sid, 0x22));
    let r = c.expect(CA_PROTO_READ_NOTIFY, "READ_NOTIFY while put pending");
    let read_at = started.elapsed();
    assert_eq!(payload_double(&r), 1.5, "the ao still serves its value");
    assert!(
        read_at < ODLY,
        "the READ_NOTIFY must be answered before the ODLY delay elapses \
         (answered after {read_at:?}, ODLY is {ODLY:?}); a blocked message \
         thread would serialise it behind the put-callback"
    );

    // The put-callback itself lands only after the delay, on the executor.
    let wn = c.expect(CA_PROTO_WRITE_NOTIFY, "deferred WRITE_NOTIFY");
    let done_at = started.elapsed();
    assert_eq!(
        ioid_of(&wn),
        0x21,
        "deferred WRITE_NOTIFY reply echoes our ioid"
    );
    assert!(
        done_at >= ODLY,
        "the put-callback must not complete before the ODLY delay \
         (completed after {done_at:?}, ODLY is {ODLY:?})"
    );

    drop(c);
    server.shutdown();
    accept.join().unwrap();
}

/// Two concurrent circuits against the real database, each with its own
/// subscription: a write on one is seen by the other. Proves the blocking
/// driver's per-client threads share one database and one event fan-out.
#[test]
fn two_circuits_share_the_database_and_its_monitors() {
    let (server, addr, accept) = start_server(build_real_db());

    let mut writer = connect_and_handshake(addr);
    let mut reader = connect_and_handshake(addr);
    let w_sid = create_channel(&mut writer, 1, "RT:AO");
    let r_sid = create_channel(&mut reader, 2, "RT:AO");

    let sub_id = 0x5C;
    reader.send(&event_add_frame(r_sid, sub_id));
    let initial = reader.expect(CA_PROTO_EVENT_ADD, "reader initial");
    assert_eq!(payload_double(&initial), 1.5);

    // The two replies land on *different* circuits here — the acknowledgement
    // on the writer, the fan-out on the reader — so there is no race to lose.
    writer.send(&write_frame(CA_PROTO_WRITE_NOTIFY, w_sid, 0x31, 12.5));
    let _ = writer.expect(CA_PROTO_WRITE_NOTIFY, "writer WRITE_NOTIFY");

    let update = reader.expect(CA_PROTO_EVENT_ADD, "reader update");
    assert_eq!(
        payload_double(&update),
        12.5,
        "the other circuit's write reaches this circuit's subscription"
    );

    drop(writer);
    drop(reader);
    server.shutdown();
    accept.join().unwrap();
}
