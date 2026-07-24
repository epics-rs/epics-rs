//! End-to-end `Db State` device support (base `devBiDbState.c` /
//! `devBoDbState.c`): a `bo` with `DTYP="Db State"` sets a process-global named
//! bit in the `dbState` registry, and a `bi` sharing the same INST_IO state
//! name reads it back. The DTYP is not a soft channel
//! (`is_soft_dtyp("Db State")` is false), so without the builtin dynamic
//! factory the records would have no device.
//!
//! These assert the BIT VALUE round-trips (bo write → registry → bi read), not
//! merely that processing leaves the records un-alarmed — a no-alarm-only test
//! would pass even if the device never ran.

use std::collections::{HashMap, HashSet};

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

/// bo sets the named bit, bi reads it back — both set (1) and clear (0)
/// round-trip through the shared `dbState` registry.
#[epics_macros_rs::epics_test]
async fn db_state_bo_write_propagates_to_bi_read() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"
record(bo, "DBST_BO") {
    field(DTYP, "Db State")
    field(OUT, "@DBST_E2E")
}
record(bi, "DBST_BI") {
    field(DTYP, "Db State")
    field(INP, "@DBST_E2E")
}
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    // bo VAL=1 → process → the shared state DBST_E2E is set.
    db.put_pv_no_process("DBST_BO.VAL", EpicsValue::Enum(1))
        .await
        .unwrap();
    let mut v = HashSet::new();
    db.process_record_with_links("DBST_BO", &mut v, 0)
        .await
        .unwrap();

    // bi process → reads the bit into VAL (device read, skip-convert).
    let mut v = HashSet::new();
    db.process_record_with_links("DBST_BI", &mut v, 0)
        .await
        .unwrap();

    let bi = db.get_record("DBST_BI").expect("bi exists");
    {
        let inst = bi.read();
        assert_ne!(
            inst.common.sevr,
            AlarmSeverity::Invalid,
            "valid Db State bi must not be INVALID"
        );
        assert_eq!(
            inst.record.get_field("VAL"),
            Some(EpicsValue::Enum(1)),
            "bi must read back the bit set by bo"
        );
    }

    // bo VAL=0 → process → state cleared; bi reads it back as 0.
    db.put_pv_no_process("DBST_BO.VAL", EpicsValue::Enum(0))
        .await
        .unwrap();
    let mut v = HashSet::new();
    db.process_record_with_links("DBST_BO", &mut v, 0)
        .await
        .unwrap();
    let mut v = HashSet::new();
    db.process_record_with_links("DBST_BI", &mut v, 0)
        .await
        .unwrap();
    {
        let inst = bi.read();
        assert_eq!(
            inst.record.get_field("VAL"),
            Some(EpicsValue::Enum(0)),
            "bi must read back the bit cleared by bo"
        );
    }
}

/// C registers a Db State dset only for `bi`/`bo`; an `ai` with
/// `DTYP="Db State"` has no matching support. The Rust device's record-type
/// gate Errs in `init()`, so `wire_device_to_record` flags the record INVALID
/// at build time — proving the device attached and gated rather than being
/// silently accepted as a soft channel.
#[epics_macros_rs::epics_test]
async fn db_state_wrong_record_type_is_invalid() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"
record(ai, "DBST_AI") {
    field(DTYP, "Db State")
    field(INP, "@SOMESTATE")
}
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let ai = db.get_record("DBST_AI").expect("ai exists");
    let inst = ai.read();
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "ai with DTYP=Db State must be INVALID (no Db State device support for ai)"
    );
}
