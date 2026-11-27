//! A simulated read converts SIOL straight into `bptr` (`mcaRecord.c:1117`):
//! `getString<Type>` (dbConvert.c) stops at the first string it cannot
//! parse, so the channels before it are written and the rest keep what they
//! held, while a DB link's `nRequest` becomes NORD whatever the status
//! (`:1119`). The status raises LINK/INVALID and NACK (`:783-786`). Every
//! value below is the C oracle IOC's (`oracleMcaIoc`, `dbgf`) on this db.

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — it ran and passed on
// the exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run
// -p mca-rs`); nothing here builds a CA server, and the reactor comes from
// `#[tokio::test]` itself, which the backend does not remove.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, DbLinkType, DevSup, Dset};
use epics_base_rs::types::{EpicsValue, PvString};

const DB: &str = r#"
record(waveform, "SW") { field(FTVL, "STRING") field(NELM, "3") }
record(mca, "MCD") { field(NMAX, "4") field(NUSE, "4") field(FTVL, "LONG") field(SIMM, "YES") field(SIOL, "SW") }
"#;

/// `mcaSupport.dbd`'s `device(mca,CONSTANT,devMCA_soft,"Soft Channel")`.
static SOFT_MCA: DevSup = DevSup {
    name: "devMCA_soft",
    choice: "Soft Channel",
    link_type: DbLinkType::Constant,
    dset: Dset::new(6, &[2, 4, 5], false),
};

async fn build() -> Arc<PvDatabase> {
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
    db
}

/// `SW` holds `items`, then a read pass runs `readValue`.
async fn read_from(db: &PvDatabase, items: &[&str]) {
    let items = items.iter().map(|s| PvString::from(*s)).collect();
    db.put_pv("SW", EpicsValue::StringArray(items))
        .await
        .unwrap();
    db.put_record_field_from_ca_no_notify("MCD", "READ", EpicsValue::Short(1))
        .await
        .unwrap();
}

/// What `dbgf MCD` serves: NORD channels.
fn served(db: &PvDatabase) -> (Vec<i32>, i32, i16, u16, AlarmSeverity) {
    let EpicsValue::Long(nord) = db.get_pv("MCD.NORD").unwrap() else {
        panic!("NORD is a LONG");
    };
    let EpicsValue::LongArray(val) = db.get_pv("MCD").unwrap() else {
        panic!("VAL is FTVL LONG");
    };
    let nack = db.get_pv("MCD.NACK").unwrap().to_f64().unwrap() as i16;
    let rec = db.get_record("MCD").unwrap();
    let inst = rec.read();
    (
        val[..nord as usize].to_vec(),
        nord,
        nack,
        inst.common.stat,
        inst.common.sevr,
    )
}

#[tokio::test]
async fn a_failed_siol_read_writes_the_channels_before_the_failed_one() {
    let db = build().await;
    read_from(&db, &["1", "2", "3"]).await;
    assert_eq!(
        served(&db),
        (
            vec![1, 2, 3],
            3,
            0,
            alarm_status::NO_ALARM,
            AlarmSeverity::NoAlarm
        )
    );

    read_from(&db, &["9", "x", "y"]).await;
    assert_eq!(
        served(&db),
        (
            vec![9, 2, 3],
            3,
            1,
            alarm_status::LINK_ALARM,
            AlarmSeverity::Invalid
        )
    );

    read_from(&db, &["5", "bad"]).await;
    assert_eq!(
        served(&db),
        (
            vec![5, 2],
            2,
            1,
            alarm_status::LINK_ALARM,
            AlarmSeverity::Invalid
        )
    );
}
