//! A record type's DTYP menu is its `device()` lines in the order they arrive,
//! so std's devTimeOfDay choices sit after asyn's when the IOC wires asyn's
//! devices before std's, even with std's record types registered first.
//! Ground truth: `oracleIoc` (`~/work/epics-modules/oracle-ioc`: base, asyn,
//! std, calc), its `dbDumpDevice` of ai and stringin, addresses masked.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::types::EpicsValue;

mod common;
use common::masked;

const AI: &str = include_str!("oracle/oracleIoc_dbDumpDevice_ai.txt");
const STRINGIN: &str = include_str!("oracle/oracleIoc_dbDumpDevice_stringin.txt");

/// The pin IOC's order: std's record types, then asyn's devices, then std's.
fn pin_ioc_builder() -> IocBuilder {
    let mut builder = IocBuilder::new();
    for (name, factory) in std_rs::std_record_factories() {
        builder = builder.register_record_type(name, move || factory());
    }
    builder = asyn_rs::adapter::register_asyn_device_support_for_builder(builder);
    for (record_type, devsup, factory) in std_rs::std_device_supports() {
        builder = builder.register_device_support(record_type, devsup, factory);
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
    assert_eq!(masked(&dump("ai")), masked(AI));
    assert_eq!(masked(&dump("stringin")), masked(STRINGIN));
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
