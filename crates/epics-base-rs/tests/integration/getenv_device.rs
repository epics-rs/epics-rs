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

use std::collections::HashMap;

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

    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("GETENV_SI", &mut visited)
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

/// C registers a getenv dset only for `stringin`/`lsi`, so `ai`'s device
/// menu has no `getenv` and the `.db` does not load (`dbStaticRun.c:499`).
#[epics_macros_rs::epics_test]
async fn getenv_wrong_record_type_is_refused_at_load() {
    let built = IocBuilder::new()
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
        .await;
    assert!(built.is_err(), "ai has no getenv device support");
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

    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("GETENV_UNSET", &mut visited)
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

    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("GETENV_UDFS", &mut visited)
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

/// C raises the unset-var alarm with `recGblSetSevrMsg(..., "No such ENV")`;
/// softIoc R7.0.10 reads AMSG "No such ENV" on both a stringin and an lsi.
#[epics_macros_rs::epics_test]
async fn getenv_unset_var_sets_amsg_like_softioc() {
    // SAFETY: nextest isolates each test in its own process (see module doc).
    unsafe {
        std::env::remove_var("GETENV_E2E_AMSG_UNSET");
    }

    let (db, _) = IocBuilder::new()
        .db_string(
            r#"
record(stringin, "GETENV_AMSG_SI") {
    field(DTYP, "getenv")
    field(INP, "@GETENV_E2E_AMSG_UNSET")
}
record(lsi, "GETENV_AMSG_LSI") {
    field(DTYP, "getenv")
    field(INP, "@GETENV_E2E_AMSG_UNSET")
    field(SIZV, "40")
}
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    for name in ["GETENV_AMSG_SI", "GETENV_AMSG_LSI"] {
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        db.process_record_with_links(name, &mut visited)
            .await
            .unwrap();
        let rec = db.get_record(name).expect("record exists");
        let inst = rec.read();
        assert_eq!(inst.common.stat, alarm_status::UDF_ALARM, "{name} STAT");
        assert_eq!(inst.common.amsg, "No such ENV", "{name} AMSG");
    }
}

/// softIoc R7.0.10 after one process of each: a set variable leaves UDF 0 and
/// an lsi LEN of `strlen` ("/opt/epics" → 10), an unset one UDF 1 and LEN 1
/// (devEnviron.c:56-66, :109-117). Before the process both read UDF 1.
#[epics_macros_rs::epics_test]
async fn getenv_udf_and_len_match_softioc() {
    // SAFETY: nextest isolates each test in its own process (see module doc).
    unsafe {
        std::env::set_var("GETENV_E2E_UDF_SET", "/opt/epics");
        std::env::remove_var("GETENV_E2E_UDF_UNSET");
    }

    let (db, _) = IocBuilder::new()
        .db_string(
            r#"
record(stringin, "SI") { field(DTYP, "getenv") field(INP, "@GETENV_E2E_UDF_SET") }
record(lsi, "LI") { field(DTYP, "getenv") field(INP, "@GETENV_E2E_UDF_SET") field(SIZV, "64") }
record(stringin, "SU") { field(DTYP, "getenv") field(INP, "@GETENV_E2E_UDF_UNSET") }
record(lsi, "LU") { field(DTYP, "getenv") field(INP, "@GETENV_E2E_UDF_UNSET") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    for (name, udf, len) in [
        ("SI", 0, None),
        ("LI", 0, Some(10)),
        ("SU", 1, None),
        ("LU", 1, Some(1)),
    ] {
        assert_eq!(
            db.get_record(name).unwrap().read().common.udf,
            1,
            "{name} UDF at init"
        );
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        db.process_record_with_links(name, &mut visited)
            .await
            .unwrap();
        let rec = db.get_record(name).unwrap();
        let inst = rec.read();
        assert_eq!(inst.common.udf, udf, "{name} UDF");
        if let Some(len) = len {
            assert_eq!(
                inst.record.get_field("LEN"),
                Some(EpicsValue::ULong(len)),
                "{name} LEN"
            );
        }
    }
}
