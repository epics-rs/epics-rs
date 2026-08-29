//! A CA `DBR_CHAR` payload is `epicsUInt8`, on every client decode path.
//!
//! `modules/ca/src/client/db_access.h:40` is `typedef epicsUInt8
//! dbr_char_t;` — the CA wire's CHAR row is UNSIGNED, unlike the
//! `DBF_CHAR` database field it shares a name with (`epicsInt8`,
//! `epicsTypes.h:44`). The client decoded every wire code through
//! `DbFieldType::from_u16`, which answers the DATABASE question, so a byte
//! the wire called 200 became `-56` the moment it was widened.
//!
//! libca has no auto-coercing put, so C never trips over this. This client
//! does: `CaChannel::put` runs `value.convert_to(snap.native_type)` before
//! framing, so a value read from a `FTVL=CHAR` waveform and re-put into a
//! `FTVL=LONG` one carried the sign extension across. `caget B` answered
//! `B 1 -56` against the Rust pair and `B 1 200` against the C pair.
//!
//! The rule now lives once, in `DbFieldType::wire_carrier`, and the three
//! client decode paths compose it: the plain READ_NOTIFY scalar
//! (`transport.rs` `make_read_reply`), its array half (`client/mod.rs`
//! `decode_plain_read_reply`), the EVENT_ADD payload (`subscription.rs`
//! `on_monitor_data`), and the compound layouts through `decode_dbr`. All
//! four are driven here, because one rule with four call sites is only
//! closed if every site is pinned.
//!
//! Display is deliberately NOT affected: C's `val2str` narrows every
//! DBR_CHAR element through a plain `char ch` before `sprintf("%d")`
//! (`ca/src/tools/tool_lib.c:114`, `:160-161`), so C's own `caget` prints
//! -56 for the same byte. That narrowing now lives in the formatter, where
//! C puts it, instead of in the carrier.

#![cfg(tokio_backend)]
#![cfg(all(feature = "client-core", not(epics_embedded_target)))]

use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::server::snapshot::DbrClass;
use epics_base_rs::types::DbFieldType;
use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::{CaChannel, CaClient};
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// The trigger byte: 0xC8 is 200 unsigned, -56 signed.
const WIRE: [u8; 2] = [0xC8, 0xC9];
/// What C's `putUcharLong` lands in a wider field.
const UNSIGNED: [i32; 2] = [200, 201];

const BYTES_PV: &str = "CLI:BYTES";
const LONGS_PV: &str = "CLI:LONGS";

/// Point a soon-to-be-constructed `CaClient` at exactly this server so it
/// skips UDP search.
///
/// SAFETY: every test in this file is `#[serial]`, so nothing else mutates
/// the environment concurrently, and the env is set before `CaClient::new`
/// snapshots its resolver configuration.
fn point_client_at(port: u16) {
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
}

/// A CHAR waveform seeded with [`WIRE`] and an empty LONG waveform, with a
/// connected channel to each. The server binds an ephemeral port.
async fn pair() -> (CaClient, CaChannel, CaChannel) {
    let len = WIRE.len() as i32;
    let server = CaServer::builder()
        .port(0)
        .record(BYTES_PV, WaveformRecord::new(len, DbFieldType::Char))
        .record(LONGS_PV, WaveformRecord::new(len, DbFieldType::Long))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });

    point_client_at(port);
    let client = CaClient::new().await.expect("client");
    let src = client.create_channel(BYTES_PV);
    let dst = client.create_channel(LONGS_PV);
    for ch in [&src, &dst] {
        ch.wait_connected(budget::FACT_BUDGET)
            .await
            .expect("connect");
    }
    // Seed over the wire, so the record holds exactly the bytes a C IOC
    // would: the server's own WRITE path already puts DBR_CHAR unsigned.
    src.put(&EpicsValue::CharArray(WIRE.to_vec()))
        .await
        .expect("seed CHAR waveform");
    (client, src, dst)
}

/// Re-put `value` into the LONG waveform and read back what landed —
/// `CaChannel::put`'s `convert_to(native_type)` is where the sign
/// extension used to happen.
async fn round_trip(dst: &CaChannel, value: &EpicsValue) -> Vec<i32> {
    dst.put(value).await.expect("put into LONG waveform");
    match dst.get().await.expect("read back LONG waveform").1 {
        EpicsValue::LongArray(v) => v,
        other => panic!("expected a LONG waveform readback, got {other:?}"),
    }
}

#[tokio::test]
#[serial]
async fn a_plain_get_of_a_char_waveform_re_puts_unsigned() {
    let (_client, src, dst) = pair().await;
    let (_, value) = src.get().await.expect("plain get");
    assert_eq!(
        round_trip(&dst, &value).await.as_slice(),
        UNSIGNED,
        "dbr_char_t is epicsUInt8; a re-put must widen through putUcharLong"
    );
}

/// The scalar half of the same decode: `make_read_reply` takes the
/// `count == 1` fast path, a different line from the array decode above.
#[tokio::test]
#[serial]
async fn a_single_element_get_takes_the_same_carrier() {
    let (_client, src, _dst) = pair().await;
    let (_, value) = src
        .get_with_timeout_count(budget::FACT_BUDGET, 1u32)
        .await
        .expect("one-element get");
    assert_eq!(
        value,
        EpicsValue::UChar(WIRE[0]),
        "the count==1 read reply must decode to the wire carrier, not DBF_CHAR"
    );
}

#[tokio::test]
#[serial]
async fn a_monitor_of_a_char_waveform_re_puts_unsigned() {
    let (_client, src, dst) = pair().await;
    let mut mon = src.subscribe().await.expect("subscribe");
    let snap = tokio::time::timeout(budget::FACT_BUDGET, mon.recv())
        .await
        .expect("monitor event within 3s")
        .expect("monitor stream open")
        .expect("monitor event");
    assert_eq!(
        round_trip(&dst, &snap.value).await.as_slice(),
        UNSIGNED,
        "the EVENT_ADD payload is the same dbr_char_t as the read reply"
    );
}

/// The compound layouts (`DBR_TIME_CHAR` here) skip a metadata header and
/// then decode the same value member, so they must answer identically.
#[tokio::test]
#[serial]
async fn a_compound_char_get_re_puts_unsigned() {
    let (_client, src, dst) = pair().await;
    let snap = src
        .get_with_metadata(DbrClass::Time)
        .await
        .expect("DBR_TIME_CHAR get");
    assert_eq!(
        round_trip(&dst, &snap.value).await.as_slice(),
        UNSIGNED,
        "decode_dbr must take the wire carrier like the plain path"
    );
}

/// The half that must NOT move: C's tools print the signed reading.
#[tokio::test]
#[serial]
async fn caget_still_renders_the_signed_reading() {
    use epics_ca_rs::cli::{CountPrefix, ValueFormat, format_value};

    let (_client, src, _dst) = pair().await;
    let (_, value) = src.get().await.expect("plain get");

    let fmt = ValueFormat::default();
    assert_eq!(
        format_value(&value, &fmt, None, CountPrefix::IfRequestedOrArray),
        "2 -56 -55",
        "val2str narrows each element through `char ch` before %d"
    );

    let long_string = ValueFormat {
        char_array_as_string: true,
        ..ValueFormat::default()
    };
    assert_eq!(
        format_value(&value, &long_string, None, CountPrefix::Never),
        "\\xc8\\xc9",
        "the -S long-string form still recognises the wire byte carrier"
    );
}

#[path = "common/budget.rs"]
mod budget;
