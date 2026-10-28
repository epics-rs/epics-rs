//! devAsynXXXArray's one `process()` serves input and output and maps a
//! failed transfer with the READ_ALARM default (devAsynXXXArray.cpp:330-332),
//! so a failed array write is READ/INVALID, where the scalar output dsets'
//! WRITE_ALARM default (devAsynInt32.c:1009) does not apply.
#![cfg(feature = "epics")]

use std::time::Duration;

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, ScanType};
use epics_base_rs::server::records::waveform::{ArrayKind, WaveformRecord};
use epics_base_rs::types::{DbFieldType, EpicsValue};

struct Refusing(PortDriverBase);

impl PortDriver for Refusing {
    fn base(&self) -> &PortDriverBase {
        &self.0
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.0
    }
    fn write_int32_array(&mut self, _: &AsynUser, _: &[i32]) -> AsynResult<()> {
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "refused".into(),
        })
    }
}

/// The alarm a put to `record_type` raises over a port whose array writes
/// fail, bound as C binds it: `aao` through OUT, `waveform` (C
/// `asynInt32ArrayWfOut`) through INP.
async fn failed_write(port: &str, can_block: bool, record_type: &str) -> (u16, AlarmSeverity) {
    let mut base = PortDriverBase::new(
        port,
        1,
        PortFlags {
            can_block,
            ..PortFlags::default()
        },
        InterfaceMask::INT32_ARRAY | InterfaceMask::DRV_USER,
        InterfaceMask::NONE,
    );
    base.create_param("ARR", ParamType::Int32Array).unwrap();
    let (runtime, _actor) =
        create_port_runtime_unclaimed(Refusing(base), RuntimeConfig::default()).unwrap();
    asyn_rs::registry::register_port(
        port,
        runtime.port_handle().clone(),
        &asyn_rs::registry::open_owner(),
    )
    .unwrap();
    let mut rec = WaveformRecord::new(4, DbFieldType::Long);
    let aao = record_type == "aao";
    if aao {
        rec.kind = ArrayKind::Aao;
    }
    let link = format!("@asyn({port},0,1)ARR");
    let ctx = DeviceSupportContext {
        record_type,
        dtyp: "asynInt32ArrayOut",
        inp: if aao { "" } else { &link },
        out: if aao { &link } else { "" },
    };
    let mut dev = asyn_rs::adapter::universal_asyn_factory(&ctx).expect("the factory binds");
    dev.set_record_info("A:ARR", ScanType::Passive);
    dev.init(&mut rec).unwrap();
    let db = PvDatabase::new();
    // C `callbackInit`: no build arms the bands for a bare database.
    epics_base_rs::runtime::task::background_init();
    db.add_record("A:ARR", Box::new(rec)).await.unwrap();
    db.ioc_init().await;
    {
        let cell = db.get_record("A:ARR").unwrap();
        let mut inst = cell.write();
        inst.common.dtyp = "asynInt32ArrayOut".into();
        inst.common.udf = 0;
        inst.device = Some(dev);
    }
    let _ = db
        .put_record_field_from_ca_no_notify(
            "A:ARR",
            "VAL",
            EpicsValue::LongArray(vec![1, 2].into()),
        )
        .await;
    let rec = db.get_record("A:ARR").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while rec.read().is_processing() {
        assert!(
            std::time::Instant::now() < deadline,
            "the write never completed"
        );
        epics_base_rs::runtime::task::sleep(Duration::from_millis(2)).await;
    }
    let inst = rec.read();
    (inst.common.stat, inst.common.sevr)
}

#[epics_macros_rs::epics_test]
async fn a_failed_array_write_is_read_alarm() {
    for (port, can_block, record_type) in [
        ("ARR_AAO_SYNC", false, "aao"),
        ("ARR_AAO_BLOCK", true, "aao"),
        ("ARR_WF_SYNC", false, "waveform"),
        ("ARR_WF_BLOCK", true, "waveform"),
    ] {
        assert_eq!(
            failed_write(port, can_block, record_type).await,
            (alarm_status::READ_ALARM, AlarmSeverity::Invalid),
            "{port}"
        );
    }
}
