//! An `asyn:READBACK` output on a blocking port whose driver fires callbacks
//! inside the write. C devAsyn counts a callback that arrives while the queued
//! write holds the record (`numDeferredOutputCallbacks`, devAsynInt32.c:607-613)
//! and requests one process per count from the bottom of each later
//! `processXxx` (:1013-1018), so the record ends at the driver's last value.
//! Here each callback's readback pass found PACT and dropped its ring entry: a
//! busy NDFileHDF5 `WriteFile` ended at "Write" where C ends at "Done".
#![cfg(feature = "epics")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{PortRuntimeHandle, RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// Every write is a busy one: the parameter reads the written value, then
/// the idle one, each posted while the write still runs.
struct Busy {
    base: PortDriverBase,
}

impl Busy {
    fn post(&mut self, addr: i32, set: impl Fn(&mut PortDriverBase)) -> AsynResult<()> {
        set(&mut self.base);
        self.base.call_param_callbacks(addr)
    }
}

impl PortDriver for Busy {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        let (r, a) = (user.reason, user.addr);
        self.post(a, |b| b.set_int32_param(r, a, value).unwrap())?;
        self.post(a, |b| b.set_int32_param(r, a, 0).unwrap())
    }
    fn write_int64(&mut self, user: &mut AsynUser, value: i64) -> AsynResult<()> {
        let (r, a) = (user.reason, user.addr);
        self.post(a, |b| b.set_int64_param(r, a, value).unwrap())?;
        self.post(a, |b| b.set_int64_param(r, a, 0).unwrap())
    }
    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        let (r, a) = (user.reason, user.addr);
        self.post(a, |b| b.set_float64_param(r, a, value).unwrap())?;
        self.post(a, |b| b.set_float64_param(r, a, 0.0).unwrap())
    }
    fn write_uint32_digital(
        &mut self,
        user: &mut AsynUser,
        value: u32,
        mask: u32,
    ) -> AsynResult<()> {
        let (r, a) = (user.reason, user.addr);
        self.post(a, |b| b.set_uint32_param(r, a, value, mask, 0).unwrap())?;
        self.post(a, |b| b.set_uint32_param(r, a, 0, mask, 0).unwrap())
    }
    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        let (r, a) = (user.reason, user.addr);
        self.post(a, |b| b.set_string_param(r, a, data).unwrap())?;
        self.post(a, |b| b.set_string_param(r, a, "Done").unwrap())?;
        Ok(data.len())
    }
}

fn port(name: &str, db: &PvDatabase) -> PortRuntimeHandle {
    let mut base = PortDriverBase::new(
        name,
        1,
        PortFlags {
            can_block: true,
            ..PortFlags::default()
        },
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_ALL,
    );
    for (param, ty) in [
        ("I32", ParamType::Int32),
        ("I64", ParamType::Int64),
        ("F64", ParamType::Float64),
        ("U32", ParamType::UInt32Digital),
        ("MSG", ParamType::Octet),
    ] {
        base.create_param(param, ty).unwrap();
    }
    let (runtime, _actor) =
        create_port_runtime_unclaimed(Busy { base }, RuntimeConfig::default()).unwrap();
    asyn_rs::registry::register_port(
        name,
        runtime.port_handle().clone(),
        db.scan_control().interrupt_gate(),
    )
    .unwrap();
    runtime
}

/// One `asyn:READBACK` output per devAsyn file that defers.
fn records(port: &str) -> String {
    [
        ("bo", "BO", "asynInt32", format!("@asyn({port},0,1)I32")),
        (
            "int64out",
            "I64",
            "asynInt64",
            format!("@asyn({port},0,1)I64"),
        ),
        ("ao", "AO", "asynFloat64", format!("@asyn({port},0,1)F64")),
        (
            "bo",
            "BOD",
            "asynUInt32Digital",
            format!("@asynMask({port},0,0x1,1)U32"),
        ),
        (
            "stringout",
            "SO",
            "asynOctetWrite",
            format!("@asyn({port},0,1)MSG"),
        ),
    ]
    .iter()
    .map(|(rtyp, name, dtyp, out)| {
        format!(
            "record({rtyp}, \"{name}\") {{ field(DTYP, \"{dtyp}\") field(OUT, \"{out}\") \
             info(asyn:READBACK, \"1\") }}\n"
        )
    })
    .collect()
}

async fn settled(db: &PvDatabase, name: &str, want: &EpicsValue) -> EpicsValue {
    let mut got = db.get_pv(name).unwrap();
    for _ in 0..400 {
        let idle = !db.get_record(name).unwrap().read().is_processing();
        if idle && &got == want {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
        got = db.get_pv(name).unwrap();
    }
    got
}

#[epics_base_rs::epics_test]
async fn a_readback_during_a_queued_write_ends_at_the_drivers_last_value() {
    let db = Arc::new(PvDatabase::new());
    let _rt = port("RBKDEFER", &db);
    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
        .with_database(db)
        .db_string(&records("RBKDEFER"), &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    // iocInit's `interruptAccept`: the driver's callbacks deliver from here.
    db.scan_control().run();

    for (name, put, idle) in [
        ("BO", EpicsValue::Enum(1), EpicsValue::Enum(0)),
        ("I64", EpicsValue::Int64(7), EpicsValue::Int64(0)),
        ("AO", EpicsValue::Double(2.5), EpicsValue::Double(0.0)),
        ("BOD", EpicsValue::Enum(1), EpicsValue::Enum(0)),
        (
            "SO",
            EpicsValue::String("Write".into()),
            EpicsValue::String("Done".into()),
        ),
    ] {
        db.put_record_field_from_ca(name, "VAL", put).await.unwrap();
        assert_eq!(settled(&db, name, &idle).await, idle, "{name}");
    }
}
