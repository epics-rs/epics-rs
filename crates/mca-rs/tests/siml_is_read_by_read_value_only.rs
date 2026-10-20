//! mca reads SIML and SIOL inside `readValue` (`mcaRecord.c:1097-1132`), which
//! only a read pass reaches, so a plain PROC with `READ = 0` reads neither and
//! raises no simulation alarm. A read takes C's `switch` on what SIML gave.
//!
//! oracleMcaIoc (`devMCA_soft`), one `dbpf <rec>.PROC 1` each, then
//! `dbpf <rec>.READ 1` each:
//!
//! ```text
//! PROC: every record NO_ALARM, NACK 0, NORD 0, SIMM as loaded
//! READ: M:Y   SIMM YES  SIMM/MINOR    NACK 0  NORD 3  VAL 11 22 33
//!       M:X   SIMM NO   LINK/INVALID  NACK 1  NORD 0
//!       M:5   SIMM 5    SOFT/INVALID  NACK 1  NORD 0
//!       M:N   SIMM NO   NO_ALARM      NACK 0  NORD 4
//!       M:K   SIMM YES  SIMM/MAJOR    NACK 0  NORD 3  VAL 11 22 33
//!       M:SF  SIMM YES  LINK/INVALID  NACK 1
//!       M:E0  SIMM YES  NO_ALARM      NACK 0  NORD 0
//!       M:CON SIMM YES  SIMM/MINOR    NACK 0  NORD 0
//! ```

// RTEMS-EXEC-MODEL-ALLOW(1): no CA server is built; the reactor is
// `#[tokio::test]`'s own, which the exec backend does not remove.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
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
record(waveform, "W") { field(FTVL, "LONG") field(NELM, "4") field(INP, "[11, 22, 33]") }
record(waveform, "WE") { field(FTVL, "LONG") field(NELM, "4") }
record(longin, "SW1") { field(VAL, "1") }
record(longin, "SW0") { field(VAL, "0") }
record(longin, "SW5") { field(VAL, "5") }
record(stringin, "SWX") { field(VAL, "x") }
record(mca, "M:Y") { field(SIML, "SW1") field(SIOL, "W") field(SIMS, "MINOR") field(NMAX, "4") field(NUSE, "4") field(FTVL, "LONG") }
record(mca, "M:X") { field(SIML, "SWX") field(SIOL, "W") field(NMAX, "4") field(NUSE, "4") field(FTVL, "LONG") }
record(mca, "M:5") { field(SIML, "SW5") field(SIOL, "W") field(NMAX, "4") field(NUSE, "4") field(FTVL, "LONG") }
record(mca, "M:N") { field(SIML, "SW0") field(SIOL, "W") field(NMAX, "4") field(NUSE, "4") field(FTVL, "LONG") }
record(mca, "M:K") { field(SIMM, "YES") field(SIOL, "W") field(SIMS, "MAJOR") field(NMAX, "4") field(NUSE, "4") field(FTVL, "LONG") }
record(mca, "M:SF") { field(SIMM, "YES") field(SIOL, "SWX") field(NMAX, "4") field(NUSE, "4") field(FTVL, "LONG") }
record(mca, "M:E0") { field(SIMM, "YES") field(SIOL, "WE") field(NMAX, "4") field(NUSE, "4") field(FTVL, "LONG") }
record(mca, "M:CON") { field(SIMM, "YES") field(SIOL, "5") field(SIMS, "MINOR") field(NMAX, "4") field(NUSE, "4") field(FTVL, "LONG") }
"#;

const RECORDS: [&str; 8] = ["M:Y", "M:X", "M:5", "M:N", "M:K", "M:SF", "M:E0", "M:CON"];

/// SIMM, STAT, SEVR, NACK, NORD.
type Seen = (u16, u16, AlarmSeverity, i64, i64);

fn seen(db: &PvDatabase, rec: &str) -> Seen {
    let field = |f: &str| db.get_pv(format!("{rec}.{f}")).unwrap().to_f64().unwrap() as i64;
    let r = db.get_record(rec).unwrap();
    let (stat, sevr) = {
        let i = r.read();
        (i.common.stat, i.common.sevr)
    };
    (
        field("SIMM") as u16,
        stat,
        sevr,
        field("NACK"),
        field("NORD"),
    )
}

#[tokio::test]
async fn only_a_read_pass_reads_siml_and_siol() {
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
    for rec in RECORDS {
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        let _ = db.process_record_with_links(rec, &mut visited).await;
        let loaded = u16::from(matches!(rec, "M:K" | "M:SF" | "M:E0" | "M:CON"));
        let got = seen(&db, rec);
        if got != (loaded, 0, AlarmSeverity::NoAlarm, 0, 0) {
            wrong.push(format!("PROC {rec}: {got:?}"));
        }
    }

    const NO: (u16, AlarmSeverity) = (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm);
    const LINK: (u16, AlarmSeverity) = (alarm_status::LINK_ALARM, AlarmSeverity::Invalid);
    let simm = |sevr| (alarm_status::SIMM_ALARM, sevr);
    for (rec, simm_val, alarm, nack, nord) in [
        ("M:Y", 1, simm(AlarmSeverity::Minor), 0, Some(3)),
        ("M:X", 0, LINK, 1, Some(0)),
        (
            "M:5",
            5,
            (alarm_status::SOFT_ALARM, AlarmSeverity::Invalid),
            1,
            Some(0),
        ),
        ("M:N", 0, NO, 0, Some(4)),
        ("M:K", 1, simm(AlarmSeverity::Major), 0, Some(3)),
        // C sets NORD to the source's count before the conversion fails.
        ("M:SF", 1, LINK, 1, None),
        ("M:E0", 1, NO, 0, Some(0)),
        ("M:CON", 1, simm(AlarmSeverity::Minor), 0, Some(0)),
    ] {
        db.put_record_field_from_ca_no_notify(rec, "READ", EpicsValue::Short(1))
            .await
            .unwrap();
        let got = seen(&db, rec);
        let want = (simm_val, alarm.0, alarm.1, nack, nord.unwrap_or(got.4));
        if got != want {
            wrong.push(format!("READ {rec}: {got:?}, C {want:?}"));
        }
    }
    for rec in ["M:Y", "M:K"] {
        let val = db.get_pv(rec).unwrap();
        if val != EpicsValue::LongArray(vec![11, 22, 33].into()) {
            wrong.push(format!("READ {rec}.VAL: {val:?}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
