//! `boRecord.c:158-163`, `mbboRecord.c:143-170`, `mbboDirectRecord.c:128-139`
//! and `aoRecord.c:132-153` convert the RVAL a dset `init_record` left and
//! define the record only when it returned 0; 2, a dset without
//! `init_record`, and the soft channel dsets (which return 2) leave VAL and
//! UDF alone.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::{
    DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, DeviceUdf,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;

/// Leaves RVAL = 20 and returns `outcome`; `None` is a dset without an
/// `init_record` entry, whose status no record consumes.
struct Probe(Option<DeviceInitOutcome>);

impl DeviceSupport for Probe {
    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        match self.0 {
            Some(outcome) => {
                record.put_field("RVAL", EpicsValue::Long(20))?;
                Ok(outcome)
            }
            None => Ok(DeviceInitOutcome::ok()),
        }
    }

    fn has_init_record(&self) -> bool {
        self.0.is_some()
    }

    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::ok())
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "probe"
    }
}

const DTYPS: [&str; 5] = [
    "probeOk",
    "probeNoConvert",
    "probeNoConvertDefined",
    "probeDefault",
    "Soft Channel",
];

async fn ioc(rtype: &str, fields: &str) -> Arc<PvDatabase> {
    let db: String = DTYPS
        .iter()
        .enumerate()
        .map(|(i, dtyp)| {
            format!("record({rtype}, \"{rtype}:{i}\") {{ field(DTYP, \"{dtyp}\") {fields} }}\n")
        })
        .collect();
    let (db, _) = IocBuilder::new()
        .register_dynamic_device_support(|ctx: &DeviceSupportContext| {
            let outcome = match ctx.dtyp {
                "probeOk" => Some(DeviceInitOutcome::ok()),
                "probeNoConvert" => Some(DeviceInitOutcome::DoNotConvert(DeviceUdf::Untouched)),
                "probeNoConvertDefined" => {
                    Some(DeviceInitOutcome::DoNotConvert(DeviceUdf::Defined))
                }
                "probeDefault" => None,
                _ => return None,
            };
            Some(Box::new(Probe(outcome)) as Box<dyn DeviceSupport>)
        })
        .db_string(&db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

/// `(VAL, UDF)` per entry of [`DTYPS`].
fn val_udf(db: &PvDatabase, rtype: &str) -> Vec<(f64, u8)> {
    (0..DTYPS.len())
        .map(|i| {
            let rec = db.get_record(format!("{rtype}:{i}")).unwrap();
            let rec = rec.read();
            let val = rec
                .record
                .get_field("VAL")
                .and_then(|v| v.to_f64())
                .unwrap();
            (val, rec.common.udf)
        })
        .collect()
}

#[epics_macros_rs::epics_test]
async fn bo_converts_and_defines_only_on_status_zero() {
    let db = ioc("bo", "").await;
    assert_eq!(
        val_udf(&db, "bo"),
        [(1.0, 0), (0.0, 1), (0.0, 0), (0.0, 1), (0.0, 1)]
    );
}

#[epics_macros_rs::epics_test]
async fn mbbo_resolves_rval_through_the_states_only_on_status_zero() {
    let db = ioc("mbbo", r#"field(ZRVL, "10") field(ONVL, "20")"#).await;
    assert_eq!(
        val_udf(&db, "mbbo"),
        [(1.0, 0), (0.0, 1), (0.0, 0), (0.0, 1), (0.0, 1)]
    );
}

#[epics_macros_rs::epics_test]
async fn mbbo_direct_shifts_rval_only_on_status_zero() {
    let db = ioc("mbboDirect", r#"field(SHFT, "2")"#).await;
    assert_eq!(
        val_udf(&db, "mbboDirect"),
        [(5.0, 0), (0.0, 1), (0.0, 0), (0.0, 1), (0.0, 1)]
    );
}

#[epics_macros_rs::epics_test]
async fn ao_converts_rval_only_on_status_zero() {
    let db = ioc("ao", r#"field(ASLO, "0.5")"#).await;
    assert_eq!(
        val_udf(&db, "ao"),
        [(10.0, 0), (0.0, 1), (0.0, 0), (0.0, 1), (0.0, 1)]
    );
}
