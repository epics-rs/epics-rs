//! Regression tests: `caget -d <type>` must request the EXACT
//! DBR type code, not collapse it to a metadata class and re-derive the
//! value type from the channel's native type.
//!
//! C `caget` keeps the requested `dbrType` verbatim (`caget.c:172`,
//! `format == specifiedDbr`): `-d DBR_TIME_FLOAT` on a DOUBLE PV asks
//! the server for `DBR_TIME_FLOAT` (16) and receives a converted float,
//! and `-d 38`/`DBR_CLASS_NAME` reaches the record-class introspection
//! type. Pre-fix `caget-rs` mapped the token to a `DbrClass` band, so
//! the request type was re-derived as `DBR_TIME_DOUBLE` (20) and the
//! `37`/`38` codes mis-routed to a value class.
//!
//! These drive a real `CaClient` ↔ `CaServer` TCP round-trip through the
//! new `CaChannel::get_with_dbr_type`, which is the exact wire request
//! that the `caget -d` front-end issues.

// Host/tokio-only: builds the async `CaClient`/`CaServer` stack in process.
// Under `rtems-exec-model` the `runtime::task` seam routes their `spawn`
// to the background executor, whose worker has no tokio reactor, so the
// listener/transport tasks panic. The RTEMS model serves from
// `BlockingCaServer` instead, so this path is inapplicable there.
#![cfg(not(feature = "rtems-exec-model"))]

use std::time::Duration;

use epics_base_rs::server::records::bi::BiRecord;
use epics_base_rs::server::records::lsi::LsiRecord;
use epics_base_rs::server::records::printf::PrintfRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{
    DBR_CHAR, DBR_CLASS_NAME, DBR_STRING, DBR_TIME_DOUBLE, DBR_TIME_FLOAT, DBR_TIME_INT,
    DbFieldType,
};
use epics_ca_rs::EpicsValue;
use epics_ca_rs::client::{CaClient, EnumReadback};
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// Point a soon-to-be-constructed `CaClient` at exactly this server so
/// it skips UDP search.
///
/// SAFETY: every test in this file is `#[serial]`, so no other test
/// mutates the environment concurrently, and the env is set before
/// `CaClient::new()` snapshots its resolver configuration.
fn point_client_at(port: u16) {
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
}

/// Bring up a server holding one DOUBLE waveform seeded with the given
/// ramp, returning a connected client channel ready to read.
async fn server_with_double_waveform(
    pv: &'static str,
    seed: Vec<f64>,
) -> (CaClient, epics_ca_rs::client::CaChannel) {
    let len = seed.len() as i32;
    let server = CaServer::builder()
        .port(0)
        .record(pv, WaveformRecord::new(len, DbFieldType::Double))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });

    point_client_at(port);
    let client = CaClient::new().await.expect("client");
    let ch = client.create_channel(pv);
    ch.wait_connected(Duration::from_secs(3))
        .await
        .expect("connect");
    ch.put(&EpicsValue::DoubleArray(seed))
        .await
        .expect("seed waveform");
    (client, ch)
}

/// Bring up a server holding one `bi` record with the given VAL index
/// and ZNAM/ONAM state labels, returning a connected client channel.
async fn server_with_bi(
    pv: &'static str,
    val: u16,
    znam: &str,
    onam: &str,
) -> (CaClient, epics_ca_rs::client::CaChannel) {
    let mut rec = BiRecord::new(val);
    rec.znam = znam.into();
    rec.onam = onam.into();
    let server = CaServer::builder()
        .port(0)
        .record(pv, rec)
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });

    point_client_at(port);
    let client = CaClient::new().await.expect("client");
    let ch = client.create_channel(pv);
    ch.wait_connected(Duration::from_secs(3))
        .await
        .expect("connect");
    (client, ch)
}

/// (caget default): the readback type the default `caget`
/// front-end now issues for an ENUM field — `DBR_STRING` — must return
/// the state LABEL, not the numeric index (C `caget.c:178-181`,
/// server-side `getEnumString`). `bi` VAL=1 with ONAM="On" → "On".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn enum_default_readback_is_state_label() {
    let (_client, ch) = server_with_bi("A5R2:BI:LBL", 1, "Off", "On").await;

    // What the default (no -d, no -n) caget path requests for an enum.
    let snap = ch
        .get_with_dbr_type(DBR_STRING, 0)
        .await
        .expect("DBR_STRING get on enum");
    assert_eq!(
        snap.value,
        EpicsValue::String("On".into()),
        "default enum readback must be the state label, not the index"
    );
}

/// (native ENUM GET): a plain library GET (`ca_get` / the native
/// `get_with_timeout`) returns the numeric ENUM index. This is the
/// building-block primitive, NOT the `caget -n` request path: `caget -n`
/// asks the server for `DBR_TIME_INT` (C `caget.c:180`
/// `if (enumAsNr) dbrType = DBR_TIME_INT`), covered by
/// `enum_numeric_readback_via_time_int` below and the
/// `enum_cli_readback_dbr` unit tests.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn enum_native_readback_is_numeric_index() {
    let (_client, ch) = server_with_bi("A5R2:BI:NUM", 1, "Off", "On").await;

    // The native GET is the plain library API building block.
    let (dbf, value) = ch
        .get_with_timeout(Duration::from_secs(3))
        .await
        .expect("native get");
    assert_eq!(dbf, DbFieldType::Enum, "native field type is ENUM");
    assert_eq!(
        value,
        EpicsValue::Enum(1),
        "native enum readback must be the numeric index"
    );
}

/// (caget -n): the request type `caget -n` / `camonitor -n` now issues for
/// an ENUM field — `DBR_TIME_INT` (C `caget.c:180` /  `camonitor.c:158`
/// `if (enumAsNr) dbrType = DBR_TIME_INT`) — must round-trip the numeric
/// index from the server, not fall through to native `DBR_TIME_ENUM`.
/// `bi` VAL=1 → short `1`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn enum_numeric_readback_via_time_int() {
    let (_client, ch) = server_with_bi("A5R2:BI:TINT", 1, "Off", "On").await;

    // What the `-n` caget/camonitor path requests for an enum.
    let snap = ch
        .get_with_dbr_type(DBR_TIME_INT, 0)
        .await
        .expect("DBR_TIME_INT get on enum");
    assert_eq!(
        snap.value,
        EpicsValue::Short(1),
        "-n enum readback (DBR_TIME_INT) must be the numeric index as a short"
    );
}

/// (camonitor default): `subscribe_with_mask_enum_as_string(.., true)`
/// — the `camonitor` default — must deliver the ENUM value as its state
/// LABEL string (C `camonitor.c:156-160` requests `DBR_TIME_STRING`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn enum_monitor_as_string_delivers_label() {
    let (_client, ch) = server_with_bi("A5R2:BI:MONS", 1, "Off", "On").await;

    let mut mon = ch
        .subscribe_with_mask_enum_as_string(0.0, epics_ca_rs::protocol::DBE_VALUE, true)
        .await
        .expect("subscribe enum-as-string");
    let snap = tokio::time::timeout(Duration::from_secs(3), mon.recv())
        .await
        .expect("monitor event within timeout")
        .expect("monitor stream open")
        .expect("monitor snapshot");
    assert_eq!(
        snap.value,
        EpicsValue::String("On".into()),
        "camonitor default must deliver the state label"
    );
}

/// (library subscribe): the plain `subscribe_with_mask` keeps the
/// native ENUM type — the opt-in flag is OFF, so existing library and
/// gateway consumers still receive numeric enum indices (no silent
/// behaviour change).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn enum_monitor_native_delivers_index() {
    let (_client, ch) = server_with_bi("A5R2:BI:MONN", 1, "Off", "On").await;

    let mut mon = ch
        .subscribe_with_mask(0.0, epics_ca_rs::protocol::DBE_VALUE)
        .await
        .expect("subscribe native");
    let snap = tokio::time::timeout(Duration::from_secs(3), mon.recv())
        .await
        .expect("monitor event within timeout")
        .expect("monitor stream open")
        .expect("monitor snapshot");
    assert_eq!(
        snap.value,
        EpicsValue::Enum(1),
        "plain library subscribe must keep the native numeric enum"
    );
}

/// (camonitor -n): the `EnumReadback::Numeric` monitor mode — the type
/// `camonitor -n` issues — must request `DBR_TIME_INT` and deliver the ENUM
/// as a numeric INT (`EpicsValue::Short(1)`), NOT the native
/// `DBR_TIME_ENUM` (`EpicsValue::Enum(1)`) nor the state label
/// (`EpicsValue::String("On")`). C `camonitor.c:158` `if (enumAsNr)
/// ppv->dbrType = DBR_TIME_INT`. This is the subscribe-path companion to
/// the GET-path `enum_numeric_readback_via_time_int`, exercising the full
/// `subscribe_with_mask_readback_count` → coordinator →
/// `subscription_readback_dbr` chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn enum_monitor_numeric_delivers_index() {
    let (_client, ch) = server_with_bi("A5R2:BI:MONI", 1, "Off", "On").await;

    let mut mon = ch
        .subscribe_with_mask_readback_count(
            0.0,
            epics_ca_rs::protocol::DBE_VALUE,
            EnumReadback::Numeric,
            false,
            None,
        )
        .await
        .expect("subscribe enum-as-number");
    let snap = tokio::time::timeout(Duration::from_secs(3), mon.recv())
        .await
        .expect("monitor event within timeout")
        .expect("monitor stream open")
        .expect("monitor snapshot");
    assert_eq!(
        snap.value,
        EpicsValue::Short(1),
        "camonitor -n must deliver the numeric enum index as a short (DBR_TIME_INT)"
    );
}

/// A DOUBLE PV read with `-d DBR_TIME_FLOAT` returns a
/// FLOAT value, proving the exact requested code is honoured rather
/// than re-derived to the native `DBR_TIME_DOUBLE`. The companion
/// `DBR_TIME_DOUBLE` request returns DOUBLE, so the two codes are not
/// interchangeable — the type travels to the wire verbatim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn caget_dbr_type_honors_exact_value_type() {
    // 1.5/2.5/3.5 are exactly representable in both f32 and f64, so the
    // FLOAT round-trip is bit-exact and the assertion is not lossy.
    let (_client, ch) = server_with_double_waveform("CAFR4:WF:FLT", vec![1.5, 2.5, 3.5]).await;

    let as_float = ch
        .get_with_dbr_type(DBR_TIME_FLOAT, 0)
        .await
        .expect("DBR_TIME_FLOAT get");
    match &as_float.value {
        EpicsValue::FloatArray(a) => assert_eq!(a.as_slice(), &[1.5_f32, 2.5, 3.5]),
        other => panic!("-d DBR_TIME_FLOAT must yield FloatArray, got {other:?}"),
    }

    let as_double = ch
        .get_with_dbr_type(DBR_TIME_DOUBLE, 0)
        .await
        .expect("DBR_TIME_DOUBLE get");
    match &as_double.value {
        EpicsValue::DoubleArray(a) => assert_eq!(a.as_slice(), &[1.5_f64, 2.5, 3.5]),
        other => panic!("-d DBR_TIME_DOUBLE must yield DoubleArray, got {other:?}"),
    }
}

/// `-d DBR_CLASS_NAME` (38) reaches the record-class
/// introspection type and returns the record's type name. Pre-fix the
/// `38` code fell into the `_ => Plain` band and never reached the
/// server's `DBR_CLASS_NAME` handler.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn caget_dbr_type_reaches_class_name() {
    let (_client, ch) = server_with_double_waveform("CAFR4:WF:CLS", vec![0.0, 0.0]).await;

    let snap = ch
        .get_with_dbr_type(DBR_CLASS_NAME, 0)
        .await
        .expect("DBR_CLASS_NAME get");
    assert_eq!(
        snap.class_name.as_deref(),
        Some("waveform"),
        "DBR_CLASS_NAME must carry the record's type name"
    );
}

/// Boundary 1: autosize GET on a `$`-suffix channel returns exactly
/// `MAX_STRING_SIZE` (= 40) bytes — string, NUL terminator, zero-padded.
/// C `dbChannel.c:489` sets `no_elements = field_size` (= 40); the Rust
/// server must advertise 40 on CREATE_CHAN and deliver 40 on every read.
/// Pre-fix `apply_long_string` emitted `strlen+1` bytes; autosize
/// `caget PV.VAL$` returned 6 bytes instead of 40 for "hello".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn long_string_autosize_returns_40_nul_padded() {
    let server = CaServer::builder()
        .port(0)
        .record("R55:LS:AUTO", StringinRecord::new("hello"))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });

    point_client_at(port);
    let client = CaClient::new().await.expect("client");
    // The `$` suffix requests the VAL field as a DBR_CHAR array.
    let ch = client.create_channel("R55:LS:AUTO.VAL$");
    ch.wait_connected(Duration::from_secs(3))
        .await
        .expect("connect to long-string channel");

    // Autosize (count=0) must return exactly 40 elements (field_size).
    let snap = ch
        .get_with_dbr_type(DBR_CHAR, 0)
        .await
        .expect("autosize DBR_CHAR get");
    let bytes = match snap.value {
        EpicsValue::CharArray(ref b) => b.as_slice(),
        ref other => panic!("expected CharArray, got {other:?}"),
    };
    assert_eq!(
        bytes.len(),
        40,
        "autosize `$` channel must return 40 bytes (= MAX_STRING_SIZE); got {}",
        bytes.len()
    );
    // First 5 bytes are "hello", byte 5 is NUL, bytes 6-39 are zero.
    assert_eq!(&bytes[..5], b"hello", "string content must match");
    assert_eq!(bytes[5], 0, "byte after string must be NUL terminator");
    assert!(
        bytes[6..].iter().all(|&b| b == 0),
        "trailing bytes must be zero-padded"
    );
}

/// Boundary 2: a count-clamped GET on a `$` channel trims the 40-byte
/// array to the requested count (C `read_reply` dbr_size_n parity).
/// Pre-fix the clamp ran BEFORE apply_long_string so it saw
/// `EpicsValue::String::count() == 1` and the predicate `count < 1`
/// was always false — `caget -# 3 PV.VAL$` returned 40 bytes, not 3.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn long_string_count_clamp_trims_char_array() {
    let server = CaServer::builder()
        .port(0)
        .record("R55:LS:CLAMP", StringinRecord::new("hello"))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });

    point_client_at(port);
    let client = CaClient::new().await.expect("client");
    let ch = client.create_channel("R55:LS:CLAMP.VAL$");
    ch.wait_connected(Duration::from_secs(3))
        .await
        .expect("connect to long-string channel");

    // count=3 must trim the 40-element char array to 3 bytes "hel".
    let snap = ch
        .get_with_dbr_type(DBR_CHAR, 3)
        .await
        .expect("count-3 DBR_CHAR get");
    let bytes = match snap.value {
        EpicsValue::CharArray(ref b) => b.as_slice(),
        ref other => panic!("expected CharArray, got {other:?}"),
    };
    assert_eq!(
        bytes.len(),
        3,
        "`caget -# 3 PV.VAL$` must return 3 bytes; got {}",
        bytes.len()
    );
    assert_eq!(
        bytes, b"hel",
        "trimmed content must be first 3 chars of string"
    );
}

/// A long-string *record* field accessed plainly (no `$`) must advertise
/// the native type C `cvt_dbaddr` gives it: a scalar `DBF_STRING`,
/// `no_elements = 1` — NOT the `DBF_CHAR` array of its internal carrier.
/// printfRecord.c:411-413 sets `field_type = dbr_field_type = DBF_STRING`
/// and `no_elements = 1` for printf VAL; the Rust record stores the value
/// as a CHAR array, so the CA boundary decodes it to a scalar string.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn printf_val_native_type_is_scalar_dbr_string() {
    let mut rec = PrintfRecord::default();
    rec.val = "log: 42".to_string();
    let server = CaServer::builder()
        .port(0)
        .record("R3C2:PF:VAL", rec)
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });

    point_client_at(port);
    let client = CaClient::new().await.expect("client");
    let ch = client.create_channel("R3C2:PF:VAL");
    ch.wait_connected(Duration::from_secs(3))
        .await
        .expect("connect to printf VAL channel");

    assert_eq!(
        ch.native_field_type().expect("native field type"),
        DbFieldType::String,
        "printf VAL must advertise native DBF_STRING (C cvt_dbaddr), not DBF_CHAR"
    );
    assert_eq!(
        ch.element_count().expect("native element count"),
        1,
        "printf VAL native count must be 1 (a single DBR_STRING)"
    );

    // Plain access delivers the value as a scalar string, not a byte array.
    let snap = ch
        .get_with_dbr_type(DBR_STRING, 0)
        .await
        .expect("DBR_STRING get on printf VAL");
    assert_eq!(
        snap.value,
        EpicsValue::String("log: 42".into()),
        "printf VAL plain read must return the formatted string"
    );
}

/// lsi/lso VAL & OVAL share the same long-string carrier and the same C
/// `cvt_dbaddr` scalar-DBF_STRING presentation (lsiRecord.c:141-143).
/// The CA boundary keys on the record's `long_string_fields()`, so the
/// whole family — not just printf — reports native DBF_STRING/1. An
/// over-40-char value clips to the 40-byte DBR_STRING slot exactly as C
/// does (the full value is reachable only via the `$` modifier).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn lsi_val_native_type_is_scalar_dbr_string_and_clips() {
    // 45 chars: longer than MAX_STRING_SIZE (40) so the DBR_STRING slot clips.
    let long = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHI";
    let server = CaServer::builder()
        .port(0)
        .record("R3C2:LSI:VAL", LsiRecord::new(long))
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    let _h = tokio::spawn(async move { server.run().await });

    point_client_at(port);
    let client = CaClient::new().await.expect("client");
    let ch = client.create_channel("R3C2:LSI:VAL");
    ch.wait_connected(Duration::from_secs(3))
        .await
        .expect("connect to lsi VAL channel");

    assert_eq!(
        ch.native_field_type().expect("native field type"),
        DbFieldType::String,
        "lsi VAL must advertise native DBF_STRING (C cvt_dbaddr), not DBF_CHAR"
    );
    assert_eq!(
        ch.element_count().expect("native element count"),
        1,
        "lsi VAL native count must be 1"
    );

    let snap = ch
        .get_with_dbr_type(DBR_STRING, 0)
        .await
        .expect("DBR_STRING get on lsi VAL");
    let s = match snap.value {
        EpicsValue::String(ref s) => s.as_str_lossy().into_owned(),
        ref other => panic!("expected scalar String, got {other:?}"),
    };
    assert!(
        long.starts_with(&s) && s.len() <= 39,
        "lsi VAL plain read must clip to the <=39-char DBR_STRING slot; got {s:?}"
    );
}

/// A DBR code past `LAST_BUFFER_TYPE` must be refused by the CLIENT, with
/// nothing on the wire — libca `nciu::read` (`nciu.cpp:292`) and
/// `comQueSend::insertRequestWithPayLoad` (`comQueSend.cpp:323`) both throw
/// `cacChannel::badType` before the request is queued.
///
/// The cost of getting this wrong is not a wasted round trip. The server
/// treats such a type as a protocol violation and tears the circuit down
/// (`AcceptedWriteType::classify` → ECA_BADTYPE + RSRV_ERROR, C
/// `write_action`), so a request that leaves the client takes the connection
/// with it — including every other channel sharing it. The surviving read at
/// the end is what proves nothing was sent.
///
/// The scalar put is the case that regressed: the write gate's element bound
/// returned early for `count == 1`, ahead of the type check.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn a_dbr_type_past_the_protocol_bound_never_leaves_the_client() {
    use epics_base_rs::error::CaError;
    use epics_base_rs::types::LAST_BUFFER_TYPE;

    let (_client, ch) = server_with_bi("R7C1:BI:BOUND", 1, "Off", "On").await;
    let over = LAST_BUFFER_TYPE + 1;

    assert!(
        matches!(ch.get_with_dbr_type(over, 0).await, Err(CaError::UnsupportedType(t)) if t == over),
        "a read past the protocol bound is refused locally"
    );
    assert!(
        matches!(
            ch.put_as_dbr_with_timeout(over, &EpicsValue::Short(1), Duration::from_secs(3)).await,
            Err(CaError::UnsupportedType(t)) if t == over
        ),
        "a scalar put-callback past the protocol bound is refused locally"
    );
    assert!(
        matches!(
            ch.put_as_dbr_nowait(over, &EpicsValue::Short(1)).await,
            Err(CaError::UnsupportedType(t)) if t == over
        ),
        "a fire-and-forget scalar put past the protocol bound is refused locally"
    );

    // The circuit is untouched: had any of the three reached the server, it
    // would have answered ECA_BADTYPE and dropped, and this read would fail.
    let snap = ch
        .get_with_dbr_type(DBR_STRING, 0)
        .await
        .expect("the circuit survives a refused request");
    assert_eq!(snap.value, EpicsValue::String("On".into()));
}
