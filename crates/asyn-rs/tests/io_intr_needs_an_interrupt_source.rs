//! An `I/O Intr` record on an interface its port's `interruptMask` leaves out
//! never processes on a driver callback. asynPortDriver registers an interrupt
//! source only for the interfaces that mask names (asynPortDriver.cpp:4074-4086,
//! asynStandardInterfacesBase.c), so devAsyn's `registerInterruptUser` fails in
//! `getInterruptPvt` (asynManager.c:2492-2497), `getIoIntInfo` prints and
//! returns 0, and the record sits on a scan list nothing requests.
#![cfg(feature = "epics")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use asyn_rs::param::{ParamType, ParamValue};
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::request::ParamSetValue;
use asyn_rs::runtime::{PortRuntimeHandle, RuntimeConfig, create_port_runtime_unclaimed};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

struct Device {
    base: PortDriverBase,
}

impl PortDriver for Device {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

const INTERFACES: InterfaceMask = InterfaceMask::DRV_USER
    .or(InterfaceMask::INT32)
    .or(InterfaceMask::INT64)
    .or(InterfaceMask::FLOAT64)
    .or(InterfaceMask::UINT32_DIGITAL)
    .or(InterfaceMask::OCTET)
    .or(InterfaceMask::INT32_ARRAY);

const PARAMS: [(&str, ParamType); 6] = [
    ("I32", ParamType::Int32),
    ("I64", ParamType::Int64),
    ("F64", ParamType::Float64),
    ("U32", ParamType::UInt32Digital),
    ("MSG", ParamType::Octet),
    ("I32A", ParamType::Int32Array),
];

fn port(name: &str, interrupts: InterfaceMask, db: &PvDatabase) -> PortRuntimeHandle {
    let mut base = PortDriverBase::new(name, 1, PortFlags::default(), INTERFACES, interrupts);
    for (param, ty) in PARAMS {
        base.create_param(param, ty).unwrap();
    }
    let (runtime, _actor) =
        create_port_runtime_unclaimed(Device { base }, RuntimeConfig::default()).unwrap();
    asyn_rs::registry::register_port(
        name,
        runtime.port_handle().clone(),
        db.scan_control().interrupt_gate(),
    )
    .unwrap();
    runtime
}

/// One `I/O Intr` input per devAsyn file, named `<prefix><rec>`.
fn records(port: &str, prefix: &str) -> String {
    [
        ("longin", "LI", "asynInt32", "I32", ""),
        ("int64in", "I64", "asynInt64", "I64", ""),
        ("ai", "AI", "asynFloat64", "F64", ""),
        ("bi", "BI", "asynUInt32Digital", "U32", ""),
        ("stringin", "SI", "asynOctetRead", "MSG", ""),
        ("waveform", "WI", "asynInt32ArrayIn", "I32A", "LONG"),
    ]
    .iter()
    .map(|(rtyp, name, dtyp, param, ftvl)| {
        let inp = if *dtyp == "asynUInt32Digital" {
            format!("@asynMask({port},0,0x1)U32")
        } else {
            format!("@asyn({port},0,1){param}")
        };
        let ftvl = if ftvl.is_empty() {
            String::new()
        } else {
            format!("field(FTVL, \"{ftvl}\") field(NELM, \"4\")")
        };
        format!(
            "record({rtyp}, \"{prefix}{name}\") {{ field(DTYP, \"{dtyp}\") field(INP, \"{inp}\") \
             field(SCAN, \"I/O Intr\") {ftvl} }}\n"
        )
    })
    .collect()
}

fn fired() -> [(&'static str, EpicsValue); 6] {
    [
        ("LI", EpicsValue::Long(7)),
        ("I64", EpicsValue::Int64(1 << 40)),
        ("AI", EpicsValue::Double(2.5)),
        ("BI", EpicsValue::Enum(1)),
        ("SI", EpicsValue::String("ok".into())),
        ("WI", EpicsValue::LongArray(vec![1, 2, 3].into())),
    ]
}

async fn fire(runtime: &PortRuntimeHandle) {
    let handle = runtime.port_handle();
    let values = [
        ParamValue::Int32(7),
        ParamValue::Int64(1 << 40),
        ParamValue::Float64(2.5),
        ParamValue::UInt32Digital(1),
        ParamValue::Octet(Arc::from(&b"ok"[..])),
        ParamValue::Int32Array(Arc::from(vec![1, 2, 3])),
    ];
    let updates = values
        .into_iter()
        .enumerate()
        .map(|(reason, value)| ParamSetValue::Value {
            reason,
            addr: 0,
            value,
        })
        .collect();
    handle.set_params_and_notify(0, updates).await.unwrap();
}

async fn settled(db: &PvDatabase, name: &str, want: &EpicsValue) -> EpicsValue {
    let mut got = db.get_pv(name).unwrap();
    for _ in 0..200 {
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
async fn an_io_intr_record_on_an_interface_without_an_interrupt_source_never_processes() {
    let db = Arc::new(PvDatabase::new());
    let silent = port("NOINTRSRC", InterfaceMask::NONE, &db);
    let live = port("INTRSRC", INTERFACES, &db);
    let db_text = records("NOINTRSRC", "N") + &records("INTRSRC", "Y");
    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
        .with_database(db)
        .db_string(&db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();
    let idle: HashMap<&str, EpicsValue> = fired()
        .iter()
        .map(|(name, _)| (*name, db.get_pv(format!("N{name}")).unwrap()))
        .collect();

    fire(&silent).await;
    fire(&live).await;

    for (name, want) in fired() {
        assert_eq!(
            settled(&db, &format!("Y{name}"), &want).await,
            want,
            "Y{name}"
        );
    }
    for (name, _) in fired() {
        let rec = format!("N{name}");
        assert_eq!(
            db.get_pv(&rec).unwrap(),
            idle[name],
            "{rec} stays unprocessed"
        );
        assert_eq!(
            db.get_record(&rec).unwrap().read().common.udf,
            1,
            "{rec} UDF"
        );
    }
}
