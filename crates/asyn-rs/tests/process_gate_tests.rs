//! The asyn record's process-passive put gate.
//!
//! C `asynRecord.dbd` marks only the output/command fields pp(TRUE):
//! AOUT, BOUT, I32OUT, UI32OUT, F64OUT, UCMD, ACMD. Every config field
//! (PORT/ADDR/TMOD/TMOT/options/trace…) is non-pp and applied by
//! `special()` off the process path. Before the
//! `"asyn" => &["AOUT",…]` `pp_fields_for` entry the record had no entry,
//! so the gate (field_io.rs `should_process`) processed on EVERY put — and
//! asyn `process()` performs real port read/write whenever TMOD != NoI/O,
//! so a put to a config field spuriously did device I/O.
//!
//! Decisive signal: the record's FLNK fires only when `process()` runs.
//! Wire it to a self-incrementing calc; the calc's VAL is the exact count
//! of asyn process() cycles. No port is connected — `perform_io` with no
//! port returns Ok ("not connected"), so FLNK fires on a process regardless
//! of I/O, which is exactly the "did it process" question this pins.

#![cfg(tokio_backend)]
// Every case here drives the asyn record through a real CA server's put
// gate. The reactor-free `exec_backend` — selected on a host build by
// `EPICS_RS_BUILD_EXEC_BACKEND=thread`, and unconditionally on RTEMS and
// VxWorks — has no `epics_ca_rs::server::CaServer` to build, so this file
// has no subject there. `[[test]] required-features` cannot name a
// build-script cfg, which is why the gate is here and not in
// `Cargo.toml`.

use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServerBuilder;
use std::collections::HashMap;

#[tokio::test]
async fn test_asyn_non_pp_put_does_not_process() {
    let db_str = r#"
record(asyn, "TEST:ASYN") {
    field(FLNK, "TEST:ACNT")
}
record(calc, "TEST:ACNT") {
    field(INPA, "TEST:ACNT.VAL")
    field(CALC, "A+1")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("asyn", || {
            Box::new(asyn_rs::asyn_record::AsynRecord::default())
        })
        .register_record_type("calc", || {
            Box::new(epics_base_rs::server::records::calc::CalcRecord::new("A+1"))
        })
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // No process yet: the FLNK-counter calc is 0.
    assert_eq!(
        server.get("TEST:ACNT").await.unwrap(),
        EpicsValue::Double(0.0),
        "counter must be 0 before any asyn process"
    );

    // Put TMOT (the I/O timeout) — a non-pp config field. Must NOT process,
    // so FLNK must not fire and no spurious device I/O is issued.
    db.put_record_field_from_ca("TEST:ASYN", "TMOT", EpicsValue::Double(2.5))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(
        server.get("TEST:ACNT").await.unwrap(),
        EpicsValue::Double(0.0),
        "a put to non-pp TMOT must NOT process the asyn record (no spurious I/O)"
    );

    // Put AOUT (the octet output) — a pp(TRUE) field. MUST process -> FLNK
    // fires once -> counter = 1.
    db.put_record_field_from_ca("TEST:ASYN", "AOUT", EpicsValue::String("x".into()))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(
        server.get("TEST:ACNT").await.unwrap(),
        EpicsValue::Double(1.0),
        "a put to pp AOUT must process the asyn record and fire FLNK once"
    );
}
