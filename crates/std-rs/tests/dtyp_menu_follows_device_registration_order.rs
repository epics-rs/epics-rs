//! A record type's DTYP menu is its `device()` lines in the order they arrive,
//! so std's devTimeOfDay choices sit after asyn's when the IOC wires asyn's
//! devices before std's, even with std's record types registered first.
//! Ground truth: `oracleIoc.dbd` (`~/work/epics-modules/oracle-ioc`: base,
//! asyn, std, calc), lines 4983-4993 (ai) and 21980-21988 (stringin).

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::types::EpicsValue;

const AI: &str = "recordtype(ai)
\tchoice:    Soft Channel
\tlink_type: 0
\tchoice:    Raw Soft Channel
\tlink_type: 0
\tchoice:    Async Soft Channel
\tlink_type: 0
\tchoice:    Soft Timestamp
\tlink_type: 12
\tchoice:    General Time
\tlink_type: 12
\tchoice:    asynInt32
\tlink_type: 12
\tchoice:    asynInt32Average
\tlink_type: 12
\tchoice:    asynFloat64
\tlink_type: 12
\tchoice:    asynFloat64Average
\tlink_type: 12
\tchoice:    asynInt64
\tlink_type: 12
\tchoice:    Sec Past Epoch
\tlink_type: 0
";

const STRINGIN: &str = "recordtype(stringin)
\tchoice:    Soft Channel
\tlink_type: 0
\tchoice:    Async Soft Channel
\tlink_type: 0
\tchoice:    Soft Timestamp
\tlink_type: 12
\tchoice:    General Time
\tlink_type: 12
\tchoice:    getenv
\tlink_type: 12
\tchoice:    asynOctetCmdResponse
\tlink_type: 12
\tchoice:    asynOctetWriteRead
\tlink_type: 12
\tchoice:    asynOctetRead
\tlink_type: 12
\tchoice:    Time of Day
\tlink_type: 0
";

/// The pin IOC's order: std's record types, then asyn's devices, then std's.
fn pin_ioc_builder() -> IocBuilder {
    let mut builder = IocBuilder::new();
    for (name, factory) in std_rs::std_record_factories() {
        builder = builder.register_record_type(name, move || factory());
    }
    builder = asyn_rs::adapter::register_asyn_device_support_for_builder(builder);
    for (dtyp, factory) in std_rs::std_device_supports() {
        builder = builder.register_device_support(dtyp, factory);
    }
    builder
}

// RTEMS-EXEC-MODEL-ALLOW(2): checked, not waived — both ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p
// std-rs --all-features --test dtyp_menu_follows_device_registration_order`).
// The shell's runtime only feeds `BlockingBridge::capture`; no CA server.
fn dump(record_type: &str) -> String {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    std::mem::forget(rt);
    let shell = IocShell::new(Arc::new(PvDatabase::new()), bridge);
    let out = tempfile::NamedTempFile::new().unwrap();
    shell
        .execute_line(&format!(
            "dbDumpDevice pdbbase {record_type} > {}",
            out.path().display()
        ))
        .unwrap();
    std::fs::read_to_string(out.path()).unwrap()
}

#[test]
fn device_dump_lists_std_after_asyn_as_oracle_ioc_dbd() {
    let _builder = pin_ioc_builder();
    assert_eq!(dump("ai"), AI);
    assert_eq!(dump("stringin"), STRINGIN);
}

#[tokio::test]
async fn dtyp_index_of_each_choice_is_its_oracle_ioc_position() {
    let db_content = "record(ai, \"AI\") {}\nrecord(stringin, \"SI\") {}\n";
    let (db, _) = pin_ioc_builder()
        .db_string(db_content, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for (rec, menu) in [("AI", AI), ("SI", STRINGIN)] {
        let choices = menu.lines().filter_map(|l| l.strip_prefix("\tchoice:    "));
        for (index, choice) in choices.enumerate() {
            db.put_record_field_from_ca_no_notify(rec, "DTYP", EpicsValue::String(choice.into()))
                .await
                .unwrap();
            assert_eq!(
                db.get_pv(&format!("{rec}.DTYP")).ok(),
                Some(EpicsValue::Enum(index as u16)),
                "{rec} DTYP {choice}"
            );
        }
    }
}
