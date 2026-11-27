//! asynInt32 and asynInt64 ai/ao recompute ESLO/EOFF on a LINR/EGUF/EGUL put while LINR is
//! LINEAR: `special_linconv` is `convertAi`/`convertAo` (devAsynInt32.c:157-161,
//! :437-473; devAsynInt64.c:153-157, :333-371) over the bounds fixed at init.
//! The port reports the `asynPortDriver` default bounds 0..65535
//! (asynPortDriver.cpp:2069-2075, :2208-2214). Expected values are
//! oracleMotorIoc (8f5015b66) asynInt32 ai/ao records on a motorSim port, the
//! same fields and the same puts; devAsynInt64's convert is the same formula.
#![cfg(feature = "epics")]

use std::collections::HashMap;
use std::sync::Arc;

use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{PortRuntimeHandle, RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const HIGH: f64 = 65535.0;

struct Bounded {
    base: PortDriverBase,
}

impl PortDriver for Bounded {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn get_bounds_int32(&self, _user: &AsynUser) -> AsynResult<(i32, i32)> {
        Ok((0, 65535))
    }
    fn get_bounds_int64(&self, _user: &AsynUser) -> AsynResult<(i64, i64)> {
        Ok((0, 65535))
    }
}

/// The database, and the port's runtime, which must outlive it.
async fn ioc(port: &str, iface: &str) -> (Arc<PvDatabase>, PortRuntimeHandle) {
    let db = Arc::new(PvDatabase::new());
    let mut base = PortDriverBase::new(
        port,
        1,
        PortFlags::default(),
        InterfaceMask::INT32 | InterfaceMask::INT64 | InterfaceMask::DRV_USER,
        InterfaceMask::NONE,
    );
    base.create_param("I32", ParamType::Int32).unwrap();
    base.create_param("I64", ParamType::Int64).unwrap();
    let param = if iface == "asynInt64" { "I64" } else { "I32" };
    let (runtime, _actor) =
        create_port_runtime_unclaimed(Bounded { base }, RuntimeConfig::default()).unwrap();
    asyn_rs::registry::register_port(
        port,
        runtime.port_handle().clone(),
        db.scan_control().interrupt_gate(),
    )
    .unwrap();
    let mut records = String::new();
    for (name, rtype, link, linr) in [
        ("AI:LIN", "ai", "INP", "LINEAR"),
        ("AI:NOC", "ai", "INP", "NO CONVERSION"),
        ("AI:SLOPE", "ai", "INP", "SLOPE"),
        ("AO:LIN", "ao", "OUT", "LINEAR"),
        ("AO:NOC", "ao", "OUT", "NO CONVERSION"),
    ] {
        records += &format!(
            "record({rtype}, \"{port}:{name}\") {{ field(DTYP, \"{iface}\") \
             field({link}, \"@asyn({port},0,1){param}\") field(LINR, \"{linr}\") \
             field(EGUL, \"5\") field(EGUF, \"10\") field(ESLO, \"2\") field(EOFF, \"3\") }}\n"
        );
    }
    let db = asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
        .with_database(db)
        .db_string(&records, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    (db, runtime)
}

fn eslo_eoff(db: &PvDatabase, name: &str) -> (f64, f64) {
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    let f = |field| {
        inst.record
            .get_field(field)
            .and_then(|v| v.to_f64())
            .unwrap()
    };
    (f("ESLO"), f("EOFF"))
}

async fn put(db: &PvDatabase, name: &str, field: &str, value: EpicsValue) {
    db.put_record_field_from_ca(name, field, value)
        .await
        .unwrap();
}

#[epics_base_rs::epics_test]
async fn int32_linconv_follows_linr_eguf_egul_puts() {
    linconv_follows_linr_eguf_egul_puts("LCI32", "asynInt32").await;
}

#[epics_base_rs::epics_test]
async fn int64_linconv_follows_linr_eguf_egul_puts() {
    linconv_follows_linr_eguf_egul_puts("LCI64", "asynInt64").await;
}

async fn linconv_follows_linr_eguf_egul_puts(p: &str, iface: &str) {
    let (db, _runtime) = ioc(p, iface).await;
    let n = |r: &str| format!("{p}:{r}");
    let init = (5.0 / HIGH, 5.0);
    let eguf20 = (15.0 / HIGH, 5.0);
    let egul1 = (19.0 / HIGH, 1.0);

    for r in ["AI:LIN", "AI:NOC", "AO:LIN", "AO:NOC"] {
        assert_eq!(eslo_eoff(&db, &n(r)), init, "{r} after init");
    }
    assert_eq!(eslo_eoff(&db, &n("AI:SLOPE")), (2.0, 3.0));

    for r in ["AI:LIN", "AI:NOC", "AI:SLOPE", "AO:LIN", "AO:NOC"] {
        put(&db, &n(r), "EGUF", EpicsValue::Double(20.0)).await;
    }
    for r in ["AI:LIN", "AO:LIN"] {
        assert_eq!(eslo_eoff(&db, &n(r)), eguf20, "{r} after EGUF 20");
    }
    for r in ["AI:NOC", "AO:NOC"] {
        assert_eq!(eslo_eoff(&db, &n(r)), init, "{r} after EGUF 20");
    }
    assert_eq!(eslo_eoff(&db, &n("AI:SLOPE")), (2.0, 3.0));

    for r in ["AI:LIN", "AO:LIN"] {
        put(&db, &n(r), "EGUL", EpicsValue::Double(1.0)).await;
        assert_eq!(eslo_eoff(&db, &n(r)), egul1, "{r} after EGUL 1");
    }
    for r in ["AI:NOC", "AO:NOC"] {
        put(&db, &n(r), "LINR", EpicsValue::Short(2)).await;
        assert_eq!(eslo_eoff(&db, &n(r)), eguf20, "{r} after LINR LINEAR");
    }
}
