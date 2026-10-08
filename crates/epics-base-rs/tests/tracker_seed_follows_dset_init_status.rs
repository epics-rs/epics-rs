//! `longinRecord.c:114-120` returns a nonzero dset `init_record` status before
//! `prec->mlst = prec->val`; `mbbiRecord.c:133-137` keeps the status and seeds
//! anyway. Both shapes of a port failure (`Err`, `Dead`) are a nonzero status.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::{
    DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, DeviceUdf,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;

struct Probe(Option<DeviceInitOutcome>);

impl DeviceSupport for Probe {
    fn init(&mut self, _record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        self.0
            .ok_or_else(|| CaError::InvalidValue("probe init fails".into()))
    }

    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "probe"
    }
}

async fn ioc() -> Arc<PvDatabase> {
    let mut db = String::new();
    for rtype in ["longin", "mbbi"] {
        for dtyp in ["probeLive", "probeDead", "probeErr"] {
            db += &format!(
                "record({rtype}, \"{rtype}:{dtyp}\") {{ field(DTYP, \"{dtyp}\") field(VAL, \"3\") }}\n"
            );
        }
    }
    let (db, _) = IocBuilder::new()
        .register_dynamic_device_support(|ctx: &DeviceSupportContext| {
            let outcome = match ctx.dtyp {
                "probeLive" => Some(DeviceInitOutcome::Live),
                "probeDead" => Some(DeviceInitOutcome::dead()),
                "probeErr" => None,
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

fn mlst(db: &PvDatabase, name: &str) -> f64 {
    db.get_record(name)
        .unwrap()
        .read()
        .record
        .get_field("MLST")
        .and_then(|v| v.to_f64())
        .unwrap()
}

#[epics_macros_rs::epics_test]
async fn longin_seeds_only_after_a_successful_dset_init_and_mbbi_always() {
    let db = ioc().await;
    for (name, want) in [
        ("longin:probeLive", 3.0),
        ("longin:probeDead", 0.0),
        ("longin:probeErr", 0.0),
        ("mbbi:probeLive", 3.0),
        ("mbbi:probeDead", 3.0),
        ("mbbi:probeErr", 3.0),
    ] {
        assert_eq!(mlst(&db, name), want, "{name}.MLST");
    }
}
