//! An `I/O Intr` input processed before any driver callback reads the device.
//! C devAsyn's process starts `if (!getCallbackValue(pPvt) && !pr->pact)
//! queueRequest` (devAsynInt32.c:836, devAsynOctet.c:757-765,
//! devAsynXXXArray.cpp:290-296): an empty ring is a polled read. Here the ring
//! was the only source, so a PINI record (delaygen DG645 `StatusLI`) stayed
//! UDF INVALID where C reads NO_ALARM.
#![cfg(feature = "epics")]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{PortRuntimeHandle, RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::PiniMode;
use epics_base_rs::types::EpicsValue;

/// A device every read reaches; nothing is ever set in the parameter library,
/// so no callback can fill a ring.
struct Device {
    base: PortDriverBase,
    reads: Arc<AtomicUsize>,
}

impl PortDriver for Device {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn read_int32(&mut self, _user: &AsynUser) -> AsynResult<i32> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(5)
    }
    fn read_int64(&mut self, _user: &AsynUser) -> AsynResult<i64> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(1 << 40)
    }
    fn read_float64(&mut self, _user: &AsynUser) -> AsynResult<f64> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(2.5)
    }
    fn read_uint32_digital(&mut self, _user: &AsynUser, mask: u32) -> AsynResult<u32> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(0xF & mask)
    }
    fn read_octet(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        buf[..2].copy_from_slice(b"ok");
        Ok(2)
    }
    fn read_int32_array(&mut self, _user: &AsynUser, buf: &mut [i32]) -> AsynResult<usize> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        buf[..3].copy_from_slice(&[1, 2, 3]);
        Ok(3)
    }
    fn read_float64_array(&mut self, _user: &AsynUser, buf: &mut [f64]) -> AsynResult<usize> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        buf[..2].copy_from_slice(&[0.5, 1.5]);
        Ok(2)
    }
}

fn port(name: &str, can_block: bool, db: &PvDatabase) -> (PortRuntimeHandle, Arc<AtomicUsize>) {
    let mut base = PortDriverBase::new(
        name,
        1,
        PortFlags {
            can_block,
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
        ("I32A", ParamType::Int32Array),
        ("F64A", ParamType::Float64Array),
    ] {
        base.create_param(param, ty).unwrap();
    }
    let reads = Arc::new(AtomicUsize::new(0));
    let (runtime, _actor) = create_port_runtime_unclaimed(
        Device {
            base,
            reads: reads.clone(),
        },
        RuntimeConfig::default(),
    )
    .unwrap();
    asyn_rs::registry::register_port(
        name,
        runtime.port_handle().clone(),
        db.scan_control().interrupt_gate(),
    )
    .unwrap();
    (runtime, reads)
}

/// Every record is `SCAN "I/O Intr"`, `PINI YES`, one per devAsyn interface.
fn records(port: &str) -> String {
    [
        ("ai", "AI32", "asynInt32", "I32", ""),
        ("ai", "AF64", "asynFloat64", "F64", ""),
        ("int64in", "I64", "asynInt64", "I64", ""),
        ("bi", "BI", "asynUInt32Digital", "U32", ""),
        ("stringin", "SI", "asynOctetRead", "MSG", ""),
        ("waveform", "WI32", "asynInt32ArrayIn", "I32A", "LONG"),
        ("waveform", "WF64", "asynFloat64ArrayIn", "F64A", "DOUBLE"),
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
            "record({rtyp}, \"{name}\") {{ field(DTYP, \"{dtyp}\") field(INP, \"{inp}\") \
             field(SCAN, \"I/O Intr\") field(PINI, \"YES\") {ftvl} }}\n"
        )
    })
    .collect()
}

const NAMES: [&str; 7] = ["AI32", "AF64", "I64", "BI", "SI", "WI32", "WF64"];

async fn settled(db: &PvDatabase) {
    for _ in 0..500 {
        if NAMES
            .iter()
            .all(|n| !db.get_record(n).unwrap().read().is_processing())
        {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("a PINI read never completed");
}

async fn pini_reads(port_name: &str, can_block: bool) {
    let db = Arc::new(PvDatabase::new());
    let (_rt, reads) = port(port_name, can_block, &db);
    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
        .with_database(db)
        .db_string(&records(port_name), &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    db.pini_process(PiniMode::Yes).await;
    settled(&db).await;

    assert_eq!(reads.load(Ordering::SeqCst), NAMES.len(), "one read each");
    for name in NAMES {
        let cell = db.get_record(name).unwrap();
        let inst = cell.read();
        assert_eq!(
            (inst.common.udf, inst.common.stat, inst.common.sevr as u16),
            (0, 0, 0),
            "{name} is defined, NO_ALARM"
        );
    }
    for (name, want) in [
        ("AI32", EpicsValue::Double(5.0)),
        ("AF64", EpicsValue::Double(2.5)),
        ("I64", EpicsValue::Int64(1 << 40)),
        ("BI", EpicsValue::Enum(1)),
        ("SI", EpicsValue::String("ok".into())),
        ("WI32", EpicsValue::LongArray(vec![1, 2, 3].into())),
        ("WF64", EpicsValue::DoubleArray(vec![0.5, 1.5].into())),
    ] {
        assert_eq!(db.get_pv(name).unwrap(), want, "{name}");
    }
}

#[epics_base_rs::epics_test]
async fn a_pini_pass_on_a_blocking_port_queues_the_read() {
    pini_reads("INTRPINIBLK", true).await;
}

#[epics_base_rs::epics_test]
async fn a_pini_pass_on_a_synchronous_port_reads_in_process() {
    pini_reads("INTRPINISYN", false).await;
}
