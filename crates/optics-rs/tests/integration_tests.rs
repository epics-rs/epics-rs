#![cfg(tokio_backend)]
// Every case here drives the table record through a real CA server's put
// gate. The reactor-free `exec_backend` — selected on a host build by
// `EPICS_RS_BUILD_EXEC_BACKEND=thread`, and unconditionally on RTEMS and
// VxWorks — has no `epics_ca_rs::server::CaServer` to build, so this file
// has no subject there. `[[test]] required-features` cannot name a
// build-script cfg, which is why the gate is here and not in
// `Cargo.toml`.

use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServerBuilder;
use optics_rs::records::table::TableRecord;
use std::collections::HashMap;

// ============================================================
// Table: a put to a non-pp field must NOT process the record.
//
// C tableRecord.dbd marks 45 fields pp(TRUE) — the user motion drives,
// the geometry params, the user limits, the action flags
// (INIT/ZERO/SYNC/READ) and the GEOM/AUNIT menus. The remaining settable
// fields (VAL, L2Z, SSET, SUSE — special(SPC_MOD) but not pp — and the
// SPC_NOMOD readbacks) are applied by `on_put` without processing.
//
// Before the `"table" => &[...]` pp_fields_for entry the record had no
// entry and ran process() on every put, so a put to a config/readback
// field spuriously entered the "Calc & Move" branch and could drive the
// motor output links. Decisive signal: table.FLNK -> a self-incrementing
// calc; the calc's VAL is the exact count of table process() cycles.
// ============================================================

#[tokio::test]
async fn test_table_non_pp_put_does_not_process() {
    let db_str = r#"
record(table, "TEST:TBL") {
    field(GEOM, "SRI")
    field(FLNK, "TEST:TCNT")
}
record(calc, "TEST:TCNT") {
    field(INPA, "TEST:TCNT.VAL")
    field(CALC, "A+1")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("table", || Box::new(TableRecord::default()))
        .register_record_type("calc", || {
            Box::new(epics_base_rs::server::records::calc::CalcRecord::new("A+1"))
        })
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // No process yet: the FLNK-counter calc is still 0.
    assert_eq!(
        server.get("TEST:TCNT").await.unwrap(),
        EpicsValue::Double(0.0),
        "counter must be 0 before any table process"
    );

    // Put VAL — a special(SPC_MOD), non-pp field (on_put is a no-op for it).
    // Must NOT process, so FLNK must not fire and the counter stays 0.
    db.put_record_field_from_ca("TEST:TBL", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(
        server.get("TEST:TCNT").await.unwrap(),
        EpicsValue::Double(0.0),
        "a put to non-pp VAL must NOT process — FLNK must not fire"
    );

    // Sanity: a real process (PROC) fires FLNK exactly once -> counter = 1.
    db.put_record_field_from_ca("TEST:TBL", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(
        server.get("TEST:TCNT").await.unwrap(),
        EpicsValue::Double(1.0),
        "PROC must process the table and fire FLNK once"
    );
}
