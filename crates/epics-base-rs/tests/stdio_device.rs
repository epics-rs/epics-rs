//! End-to-end wiring for the `stdio` built-in device support (base
//! `devStdio.c`): an `lso`/`printf`/`stringout` record with `DTYP="stdio"`
//! routes through the pre-registered builtin dynamic factory and gets a
//! `StdioDeviceSupport` wired from its INST_IO `OUT` stream name. The DTYP is
//! not a soft channel (`is_soft_dtyp("stdio")` is false), so without the
//! factory the record would have no device at all.

use std::collections::{HashMap, HashSet};

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;

/// A `stringout` with `DTYP="stdio"` and a known `OUT` stream processes
/// cleanly: the device attaches via the dynamic factory and `write()` runs
/// (printing VAL to the stream), leaving the record un-alarmed. A missing
/// factory would either leave no device or fail to resolve the stream.
#[tokio::test]
async fn stdio_stringout_known_stream_processes_clean() {
    let db_content = r#"
record(stringout, "SO_STDIO") {
    field(DTYP, "stdio")
    field(OUT, "@stdout")
    field(VAL, "hello from stdio")
}
"#;
    let (db, _) = IocBuilder::new()
        .db_string(db_content, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SO_STDIO", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("SO_STDIO").await.expect("record exists");
    let inst = rec.read().await;
    assert_ne!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "a valid stdio stringout must not be INVALID after processing"
    );
}

/// C registers a stdio dset only for `lso`/`printf`/`stringout`; a `stringin`
/// with `DTYP="stdio"` has no matching support. The Rust device's record-type
/// gate Errs in `init()`, so `wire_device_to_record` flags the record INVALID
/// at build time — proving the stdio device attached and gated, rather than
/// being silently accepted as a soft channel.
#[tokio::test]
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

    let rec = db.get_record("SI_STDIO").await.expect("record exists");
    let inst = rec.read().await;
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "stringin with DTYP=stdio must be INVALID (no stdio device support for stringin)"
    );
}
