//! End-to-end wiring for the `stdio` built-in device support (base
//! `devStdio.c`): an `lso`/`printf`/`stringout` record with `DTYP="stdio"`
//! routes through the pre-registered builtin dynamic factory and gets a
//! `StdioDeviceSupport` wired from its INST_IO `OUT` stream name. The DTYP is
//! not a soft channel (`is_soft_dtyp("stdio")` is false), so without the
//! factory the record would have no device at all.
//!
//! Each test drives a record through the real `process()` path with
//! `OUT="@errlog"` and captures the `errlog` `tracing` sink, asserting the
//! record's VAL string was actually printed — not merely that processing left
//! the record un-alarmed. That distinction matters: all three records carry
//! VAL in two shapes (`stringout` a String, `lso`/`printf` a char array) and
//! `printf` only reaches the device-write path once it is in
//! `can_device_write()`, so a "not INVALID" assertion alone would pass even
//! when nothing is printed.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::fmt::MakeWriter;

/// A `tracing` writer that appends every formatted event into a shared buffer
/// so a test can read back what the `errlog` sink received.
#[derive(Clone, Default)]
struct CaptureBuf(Arc<Mutex<Vec<u8>>>);

impl CaptureBuf {
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for CaptureBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CaptureBuf {
    type Writer = CaptureBuf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Install a thread-local subscriber capturing the `errlog` `tracing` sink for
/// the lifetime of the returned guard. `#[epics_macros_rs::epics_test]` runs the body and every
/// `.await` on a single (current-thread) runtime, so the thread-local default
/// stays active across `process_record_with_links`, where the `stdio` device's
/// `errlog_printf` is emitted inline.
fn capture_errlog() -> (DefaultGuard, CaptureBuf) {
    let buf = CaptureBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .without_time()
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (guard, buf)
}

/// Build a one-record IOC and process the named record, returning its alarm
/// severity afterwards.
async fn build_and_process(db_content: &str, record: &str) -> AlarmSeverity {
    let (db, _) = IocBuilder::new()
        .db_string(db_content, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links(record, &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record(record).expect("record exists");
    let inst = rec.read();
    inst.common.sevr
}

/// `stringout` carries a String VAL; processing must route it to the stdio
/// device and print VAL to the `errlog` stream, leaving the record un-alarmed.
#[epics_macros_rs::epics_test]
async fn stdio_stringout_prints_val_to_errlog() {
    let (_guard, buf) = capture_errlog();
    let sevr = build_and_process(
        r#"record(stringout, "SO_ERR") {
    field(DTYP, "stdio")
    field(OUT, "@errlog")
    field(VAL, "stringout to errlog")
}"#,
        "SO_ERR",
    )
    .await;

    assert_ne!(
        sevr,
        AlarmSeverity::Invalid,
        "valid stdio stringout must not be INVALID"
    );
    assert!(
        buf.contents().contains("stringout to errlog"),
        "errlog sink should contain the stringout VAL; got: {:?}",
        buf.contents()
    );
}

/// `lso` carries a `CharArray` VAL (not a String). A String-only `write()`
/// would print nothing for it; this test fails on that regression because the
/// VAL never reaches the `errlog` sink.
#[epics_macros_rs::epics_test]
async fn stdio_lso_prints_char_array_val_to_errlog() {
    let (_guard, buf) = capture_errlog();
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(lso, "LSO_ERR") {
    field(DTYP, "stdio")
    field(OUT, "@errlog")
}"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    // lso VAL is a long-string char array; the db loader parses
    // `field(VAL,"text")` as a scalar DBF_CHAR, so set VAL through the runtime
    // put path (which accepts a `CharArray`) rather than a static db field.
    db.put_pv_no_process(
        "LSO_ERR.VAL",
        EpicsValue::CharArray(b"lso to errlog".to_vec()),
    )
    .await
    .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("LSO_ERR", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("LSO_ERR").expect("record exists");
    let inst = rec.read();
    assert_ne!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "valid stdio lso must not be INVALID"
    );
    assert!(
        buf.contents().contains("lso to errlog"),
        "errlog sink should contain the lso (char-array) VAL; got: {:?}",
        buf.contents()
    );
}

/// `printf` formats FMT into VAL (a `CharArray`); its device write IS its only
/// output mechanism (`printfRecord.c:388` calls `write_string` unconditionally).
/// This fails on the regression where `printf` is absent from
/// `can_device_write()` and so never reaches `dev.write()`.
#[epics_macros_rs::epics_test]
async fn stdio_printf_routes_to_dev_write_and_prints_to_errlog() {
    let (_guard, buf) = capture_errlog();
    let sevr = build_and_process(
        r#"record(printf, "PF_ERR") {
    field(DTYP, "stdio")
    field(OUT, "@errlog")
    field(FMT, "printf to errlog")
}"#,
        "PF_ERR",
    )
    .await;

    assert_ne!(
        sevr,
        AlarmSeverity::Invalid,
        "valid stdio printf must not be INVALID"
    );
    assert!(
        buf.contents().contains("printf to errlog"),
        "errlog sink should contain the printf VAL (proves printf reaches dev.write); got: {:?}",
        buf.contents()
    );
}

/// C registers a stdio dset only for `lso`/`printf`/`stringout`; a `stringin`
/// with `DTYP="stdio"` has no matching support. The Rust device's record-type
/// gate Errs in `init()`, so `wire_device_to_record` flags the record INVALID
/// at build time — proving the stdio device attached and gated, rather than
/// being silently accepted as a soft channel.
#[epics_macros_rs::epics_test]
async fn stdio_wrong_record_type_is_invalid() {
    let db_content = r#"
record(stringin, "SI_STDIO") {
    field(DTYP, "stdio")
    field(INP, "@stdout")
}
"#;
    let (db, _) = IocBuilder::new()
        .db_string(db_content, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let rec = db.get_record("SI_STDIO").expect("record exists");
    let inst = rec.read();
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "stringin with DTYP=stdio must be INVALID (no stdio device support for stringin)"
    );
}
