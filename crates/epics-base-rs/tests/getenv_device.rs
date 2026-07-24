//! End-to-end wiring for the `getenv` built-in device support (base
//! `getenvDevSup` / `devLsiEnviron` / `devSiEnviron`): a `stringin`/`lsi`
//! record with `DTYP="getenv"` must route through the dynamic builtin factory
//! (`builtin_dynamic_factory`), get the device wired with its INST_IO `INP`,
//! and have its `VAL` written from the named environment variable — the
//! factory→`wire_device_to_record`→`read` path.
//!
//! This test caught a real latent bug: getenv was originally registered as a
//! context-free *static* factory and read the env-var name from
//! `record.get_field("INP")` — but the inner typed record handed to the device
//! does NOT expose `INP` (it lives on the `RecordInstance` common header), so
//! `INP` resolved to empty and every getenv record came up INVALID through
//! `IocBuilder`. The fix moves getenv to the dynamic factory so it receives the
//! `INP` from `DeviceSupportContext`, like its INP/OUT-needing siblings (stdio /
//! Db State / Soft Timestamp). getenv was the only one of the four base builtins
//! whose siblings all had an IOC-build e2e test while it had only direct-call
//! unit tests that bypass `IocBuilder`; this closes that asymmetry.
//!
//! `std::env::set_var` is race-free here: nextest runs each `#[test]` in its own
//! process, so the variable set below is private to this test's process.

use std::collections::{HashMap, HashSet};

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

/// A processed `stringin` with `DTYP="getenv"` and `INP="@VAR"` reads the
/// environment variable into `VAL` — proving the static builtin factory wired
/// the device and its read ran end-to-end (the pre-wiring default `VAL` is an
/// empty string, so a non-empty match proves the device ran).
#[epics_macros_rs::epics_test]
async fn getenv_stringin_reads_env_var_through_iocbuilder() {
    // SAFETY: nextest isolates each test in its own process (see module doc),
    // so this set_var cannot race another test.
    unsafe {
        std::env::set_var("GETENV_E2E_TEST_VAR", "hello-from-env");
    }

    let (db, _) = IocBuilder::new()
        .db_string(
            r#"
record(stringin, "GETENV_SI") {
    field(DTYP, "getenv")
    field(INP, "@GETENV_E2E_TEST_VAR")
}
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("GETENV_SI", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("GETENV_SI").expect("record exists");
    let inst = rec.read();
    assert_ne!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "stringin/getenv with a set env var must not be INVALID"
    );
    assert_eq!(
        inst.record.get_field("VAL"),
        Some(EpicsValue::String("hello-from-env".into())),
        "VAL must be the environment variable value read by the wired device"
    );
}

/// C registers a getenv dset only for `stringin`/`lsi`; the device's
/// record-type gate Errs in `init()`, so `wire_device_to_record` flags an
/// `ai` with `DTYP="getenv"` INVALID at build time — proving the device
/// attached and gated rather than being silently accepted.
#[epics_macros_rs::epics_test]
async fn getenv_wrong_record_type_is_invalid() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"
record(ai, "GETENV_AI") {
    field(DTYP, "getenv")
    field(INP, "@PATH")
}
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let ai = db.get_record("GETENV_AI").expect("ai exists");
    let inst = ai.read();
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "ai with DTYP=getenv must be INVALID (no getenv device support for ai)"
    );
}

/// An unset env var: C raises UDF_ALARM at UDFS (default INVALID) and clears
/// VAL, returning success (read_lsi devEnviron.c:62-67 / read_stringin
/// :114-118). base-rs surfaces the SAME UDF_ALARM via the device `last_alarm()`
/// channel — not READ_ALARM (which the earlier soft-Err path produced) and with
/// no per-cycle stderr spam.
#[epics_macros_rs::epics_test]
async fn getenv_unset_var_raises_udf_alarm_not_read_alarm() {
    // SAFETY: nextest isolates each test in its own process (see module doc), so
    // removing this variable cannot race another test.
    unsafe {
        std::env::remove_var("GETENV_E2E_DEFINITELY_UNSET_VAR");
    }

    let (db, _) = IocBuilder::new()
        .db_string(
            r#"
record(stringin, "GETENV_UNSET") {
    field(DTYP, "getenv")
    field(INP, "@GETENV_E2E_DEFINITELY_UNSET_VAR")
}
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("GETENV_UNSET", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("GETENV_UNSET").expect("record exists");
    let inst = rec.read();
    assert_eq!(
        inst.common.stat,
        alarm_status::UDF_ALARM,
        "unset env var must raise UDF_ALARM (C recGblSetSevrMsg UDF_ALARM), not \
         READ_ALARM"
    );
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "unset-var severity must be UDFS (default INVALID)"
    );
    assert_eq!(
        inst.record.get_field("VAL"),
        Some(EpicsValue::String(String::new().into())),
        "unset env var clears VAL to empty (C val[0]=0)"
    );
}

/// The unset-var alarm fires at the record's *configured* UDFS, not a hardcoded
/// INVALID — getenv captures `prec->udfs` in set_process_context, matching C
/// `recGblSetSevrMsg(prec, UDF_ALARM, prec->udfs, …)`. A `field(UDFS,"MINOR")`
/// record with an unset var must raise UDF_ALARM at MINOR. This guards the
/// reason `set_process_context` exists: drop the `ctx.udfs` capture and the
/// severity reverts to the INVALID default, failing this assertion (the
/// default-UDFS test above would still pass, so it cannot catch that regression).
#[epics_macros_rs::epics_test]
async fn getenv_unset_var_honors_user_lowered_udfs() {
    // SAFETY: nextest isolates each test in its own process (see module doc), so
    // removing this variable cannot race another test.
    unsafe {
        std::env::remove_var("GETENV_E2E_UDFS_MINOR_UNSET");
    }

    let (db, _) = IocBuilder::new()
        .db_string(
            r#"
record(stringin, "GETENV_UDFS") {
    field(DTYP, "getenv")
    field(INP, "@GETENV_E2E_UDFS_MINOR_UNSET")
    field(UDFS, "MINOR")
}
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("GETENV_UDFS", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("GETENV_UDFS").expect("record exists");
    let inst = rec.read();
    assert_eq!(
        inst.common.stat,
        alarm_status::UDF_ALARM,
        "unset env var must raise UDF_ALARM regardless of the UDFS severity"
    );
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Minor,
        "unset-var severity must follow the record's UDFS (MINOR), not a hardcoded \
         INVALID"
    );
}
