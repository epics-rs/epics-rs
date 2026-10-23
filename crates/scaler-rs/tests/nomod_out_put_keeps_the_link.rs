//! A refused CA put to scaler's `OUT` (`SPC_NOMOD`, `scalerRecord.dbd`)
//! leaves the hardware link as loaded.
//!
//! C `dbPutFieldLink` (`dbAccess.c:1160-1188`): a device link reaches
//! `dbPutSpecial(paddr, 0)` and its refusal leaves the record with no dset
//! and PACT set; a hardware link outside a device list has no lset, so
//! `dbLinkIsDefined` is false and the put is `S_db_badHWaddr` before
//! anything is touched. `dbRemoveLink` never runs on a hardware link.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record};
use epics_base_rs::types::EpicsValue;

/// An INST_IO row with a dsxt, so the put passes `S_db_noSupport`.
static HW_EXT: DevSup = DevSup {
    name: "devHwExt",
    choice: "Hw Ext",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(7, &[], true),
};

struct HwExt;

impl DeviceSupport for HwExt {
    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "Hw Ext"
    }
}

async fn loaded(builder: IocBuilder, dtyp: &str) -> Arc<PvDatabase> {
    let (name, factory) = scaler_rs::scaler_record_factory();
    builder
        .register_record_type(name, factory)
        .db_string(
            &format!(r#"record(scaler, "S") {{ {dtyp} field(OUT, "@asyn(X 0)") }}"#),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn put_out(db: &PvDatabase) -> Result<(), CaError> {
    db.put_record_field_from_ca("S", "OUT", EpicsValue::String("@asyn(Y 0)".into()))
        .await
        .map(drop)
}

fn out(db: &PvDatabase) -> EpicsValue {
    db.get_pv("S.OUT").unwrap()
}

// RTEMS-EXEC-MODEL-ALLOW(2): checked, not waived — both ran and passed on
// the exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p
// scaler-rs --all-features --test nomod_out_put_keeps_the_link`). No CA
// server is built; the reactor comes from `#[tokio::test]`, which the
// backend does not remove.
#[tokio::test]
async fn a_hardware_out_with_no_device_row_is_bad_hw_addr_and_kept() {
    let db = loaded(IocBuilder::new(), "").await;
    for _ in 0..2 {
        let r = put_out(&db).await;
        assert!(matches!(r, Err(CaError::BadLink(_))), "{r:?}");
        assert_eq!(out(&db), EpicsValue::String("@asyn(X 0)".into()));
    }
}

#[tokio::test]
async fn a_device_out_is_refused_by_nomod_and_kept() {
    let db = loaded(
        IocBuilder::new().register_device_support("scaler", &HW_EXT, || Box::new(HwExt)),
        r#"field(DTYP, "Hw Ext")"#,
    )
    .await;
    let r = put_out(&db).await;
    assert!(matches!(r, Err(CaError::ReadOnlyField(_))), "{r:?}");
    assert_eq!(out(&db), EpicsValue::String("@asyn(X 0)".into()));
    assert_eq!(db.get_pv("S.PACT").unwrap().to_f64(), Some(1.0));
}
