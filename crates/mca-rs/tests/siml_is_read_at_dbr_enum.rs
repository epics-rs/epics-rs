//! mca reads SIML itself, at `DBR_ENUM`, inside `readValue`
//! (`mcaRecord.c:1106`), which only a read cycle runs. A string source's
//! `cvt_st_e` asks the source's `put_enum_str`, which stringin has none of,
//! so even `"5"` fails the read; a number converts, and 70000 wraps to 4464,
//! which `readValue` answers with SOFT/INVALID.
//!
//! oracleMcaIoc on this database, `dbpf M:<k>.READ 1` then `dbgf`:
//!
//! ```text
//! M:five SIMM NO    LINK/INVALID "field SIML"
//! M:n1   SIMM YES   NO_ALARM
//! M:nbig SIMM 4464  SOFT/INVALID
//! ```

// RTEMS-EXEC-MODEL-ALLOW(1): no CA server is built; the reactor is
// `#[tokio::test]`'s own, which the exec backend does not remove.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, DbLinkType, DevSup, Dset};
use epics_base_rs::types::EpicsValue;

/// `mcaSupport.dbd`'s `device(mca,CONSTANT,devMCA_soft,"Soft Channel")`.
static SOFT_MCA: DevSup = DevSup {
    name: "devMCA_soft",
    choice: "Soft Channel",
    link_type: DbLinkType::Constant,
    dset: Dset::new(6, &[2, 4, 5], false),
};

const DB: &str = r#"
record(waveform, "W") { field(FTVL, "LONG") field(NELM, "4") }
record(stringin, "S:five") { field(VAL, "5") }
record(ao, "S:n1") { field(VAL, "1") }
record(ao, "S:nbig") { field(VAL, "70000") }
record(mca, "M:five") { field(SIML, "S:five") field(SIOL, "W") field(NMAX, "4") field(NUSE, "4") field(FTVL, "LONG") }
record(mca, "M:n1") { field(SIML, "S:n1") field(SIOL, "W") field(NMAX, "4") field(NUSE, "4") field(FTVL, "LONG") }
record(mca, "M:nbig") { field(SIML, "S:nbig") field(SIOL, "W") field(NMAX, "4") field(NUSE, "4") field(FTVL, "LONG") }
"#;

#[tokio::test]
async fn a_read_cycle_reads_siml_at_dbr_enum() {
    let (name, factory) = mca_rs::mca_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_device_support("mca", &SOFT_MCA, || Box::new(mca_rs::SoftMca))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let mut wrong = Vec::new();
    for (rec, simm, alarm) in [
        (
            "M:five",
            0,
            (
                AlarmSeverity::Invalid,
                alarm_status::LINK_ALARM,
                "field SIML",
            ),
        ),
        (
            "M:n1",
            1,
            (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM, ""),
        ),
        (
            "M:nbig",
            4464,
            (AlarmSeverity::Invalid, alarm_status::SOFT_ALARM, ""),
        ),
    ] {
        db.put_record_field_from_ca_no_notify(rec, "READ", EpicsValue::Short(1))
            .await
            .unwrap();
        let r = db.get_record(rec).unwrap();
        let inst = r.read();
        let got = (
            inst.record.get_field("SIMM"),
            (
                inst.common.sevr,
                inst.common.stat,
                inst.common.amsg.to_string(),
            ),
        );
        let want = (
            Some(EpicsValue::Enum(simm)),
            (alarm.0, alarm.1, alarm.2.to_string()),
        );
        if got != want {
            wrong.push(format!("{rec}: {got:?}, C {want:?}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
