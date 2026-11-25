//! A device whose `init_record` notes whether the `dbCaLink` worker is up,
//! as C's is: `iocBuild` runs `dbCaLinkInit` (`iocInit.c:216`) before
//! `initDatabase`'s `init_record` pass.

use std::collections::HashMap;
use std::sync::Mutex;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{
    DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, DeviceUdf,
};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record};

static ROW: DevSup = DevSup {
    name: "devCaLinkInitProbe",
    choice: "CaLinkInitProbe",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[], false),
};

/// Whether the worker was up at each `init_record`.
pub static SEEN: Mutex<Vec<bool>> = Mutex::new(Vec::new());

struct Probe;

impl DeviceSupport for Probe {
    fn init(&mut self, _record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        SEEN.lock().unwrap().push(worker_up());
        Ok(DeviceInitOutcome::ok())
    }

    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "CaLinkInitProbe"
    }
}

fn worker_up() -> bool {
    let up = std::cell::Cell::new(false);
    epics_base_rs::runtime::taskwd::taskwd_show(1, &|line| {
        up.set(up.get() || line.contains("dbCaLink"))
    });
    up.get()
}

/// One probed record, and an `IocBuilder` over it.
pub fn builder() -> IocBuilder {
    IocBuilder::new()
        .register_device_support("ai", &ROW, || Box::new(Probe))
        .db_string(
            r#"record(ai, "P") { field(DTYP, "CaLinkInitProbe") field(INP, "@x") }"#,
            &HashMap::new(),
        )
        .unwrap()
}
