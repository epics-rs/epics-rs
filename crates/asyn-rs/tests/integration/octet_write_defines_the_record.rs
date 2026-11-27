//! devAsynOctet's `processCommon` ends every pass of an output record with
//!
//! ```c
//! if (pPvt->result.status == asynSuccess) {
//!     pPvt->precord->udf = 0;
//! ```
//!
//! (devAsynOctet.c:817-818), so a stringout processed without a VAL put (PINI,
//! a forward link) is defined once its write lands, and stays undefined when
//! the write fails. rs left UDF at 1 after every such write.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{PortRuntimeHandle, RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

struct Writer {
    base: PortDriverBase,
    refuse: bool,
}

impl PortDriver for Writer {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        if self.refuse {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "refused".into(),
            });
        }
        Ok(data.len())
    }
}

fn port(name: &str, can_block: bool, refuse: bool, db: &PvDatabase) -> PortRuntimeHandle {
    let mut base = PortDriverBase::new(
        name,
        1,
        PortFlags {
            can_block,
            ..PortFlags::default()
        },
        InterfaceMask::OCTET | InterfaceMask::DRV_USER,
        InterfaceMask::NONE,
    );
    base.create_param("MSG", ParamType::Octet).unwrap();
    let (runtime, _actor) =
        create_port_runtime_unclaimed(Writer { base, refuse }, RuntimeConfig::default()).unwrap();
    asyn_rs::registry::register_port(
        name,
        runtime.port_handle().clone(),
        db.scan_control().interrupt_gate(),
    )
    .unwrap();
    runtime
}

async fn idle(db: &PvDatabase, name: &str) {
    for _ in 0..400 {
        if !db.get_record(name).unwrap().read().is_processing() {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("{name} never completed");
}

#[epics_base_rs::epics_test]
async fn a_landed_octet_write_defines_the_record() {
    let db = Arc::new(PvDatabase::new());
    let mut ports = Vec::new();
    let mut records = String::new();
    for (can_block, tag) in [(false, "S"), (true, "B")] {
        for (refuse, out) in [(false, "OK"), (true, "FAIL")] {
            let port_name = format!("UDF{tag}{out}");
            ports.push(port(&port_name, can_block, refuse, &db));
            records += &format!(
                r#"record(stringout, "SO_{tag}_{out}") {{
                    field(DTYP, "asynOctetWrite") field(OUT, "@asyn({port_name},0,1)MSG")
                }}
                "#
            );
        }
    }
    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
        .with_database(db)
        .db_string(&records, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();

    for tag in ["S", "B"] {
        for (out, udf) in [("OK", 0), ("FAIL", 1)] {
            let name = format!("SO_{tag}_{out}");
            assert_eq!(
                db.get_pv(format!("{name}.UDF")).unwrap(),
                EpicsValue::UChar(1),
                "{name} at boot"
            );
            // A failed synchronous write is the pass's status.
            let _ = db.process_record(&name).await;
            idle(&db, &name).await;
            assert_eq!(
                db.get_pv(format!("{name}.UDF")).unwrap(),
                EpicsValue::UChar(udf),
                "{name}"
            );
        }
    }
}
