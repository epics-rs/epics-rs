//! Only the scalar dsets keep a refused `queueRequest`'s status in
//! `result.status` (devAsynInt32.c:796). devAsynOctet.c:719 and
//! devAsynXXXArray.cpp:266 report the refusal and leave the status their last
//! `processCommon` reset to asynSuccess. asynStatusToEpicsAlarm leaves the
//! alarm alone on asynSuccess (asynEpicsUtils.c:238-240), so the refused
//! process raises the `result.alarmStatus` the last transfer left: a failed
//! transfer's alarm again, not COMM.
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
use epics_base_rs::server::record::{AlarmSeverity, Record, ScanType};
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::server::records::stringout::StringoutRecord;
use epics_base_rs::server::records::waveform::{ArrayKind, WaveformRecord};
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// A port whose writes fail.
struct Failing(PortDriverBase);

fn failed<T>() -> AsynResult<T> {
    Err(AsynError::Status {
        status: AsynStatus::Error,
        message: "failed".into(),
    })
}

impl PortDriver for Failing {
    fn base(&self) -> &PortDriverBase {
        &self.0
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.0
    }
    fn write_octet(&mut self, _: &mut AsynUser, _: &[u8]) -> AsynResult<usize> {
        failed()
    }
    fn write_int32_array(&mut self, _: &AsynUser, _: &[i32]) -> AsynResult<()> {
        failed()
    }
    fn read_octet(&mut self, _: &AsynUser, _: &mut [u8]) -> AsynResult<usize> {
        failed()
    }
    fn read_int32_array(&mut self, _: &AsynUser, _: &mut [i32]) -> AsynResult<usize> {
        failed()
    }
}

async fn settled(db: &PvDatabase) -> (u16, AlarmSeverity) {
    let rec = db.get_record("A:R").unwrap();
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

/// A record bound to a port whose transfers fail, and the put that processes it.
struct Case {
    port: &'static str,
    can_block: bool,
    dtyp: &'static str,
    param: ParamType,
    rec: Box<dyn Record>,
    /// Bound through INP, as C binds a waveform; OUT otherwise.
    inp: bool,
    put: (&'static str, EpicsValue),
}

/// The alarms of two processes: one whose transfer the port fails, then one
/// it refuses once disconnected without auto-connect.
async fn failed_then_refused(case: Case) -> [(u16, AlarmSeverity); 2] {
    let Case {
        port,
        can_block,
        dtyp,
        param,
        mut rec,
        inp,
        put: (field, val),
    } = case;
    let mut base = PortDriverBase::new(
        port,
        1,
        PortFlags {
            can_block,
            ..PortFlags::default()
        },
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_ALL,
    );
    base.create_param("P", param).unwrap();
    base.set_auto_connect(false);
    base.set_connected(true);
    let (runtime, _actor) =
        create_port_runtime_unclaimed(Failing(base), RuntimeConfig::default()).unwrap();
    asyn_rs::registry::register_port(
        port,
        runtime.port_handle().clone(),
        &asyn_rs::registry::open_owner(),
    )
    .unwrap();
    let link = format!("@asyn({port},0,1)P");
    let ctx = DeviceSupportContext {
        record_type: rec.record_type(),
        dtyp,
        inp: if inp { &link } else { "" },
        out: if inp { "" } else { &link },
    };
    let mut dev = asyn_rs::adapter::universal_asyn_factory(&ctx).expect("the factory binds");
    dev.set_record_info("A:R", ScanType::Passive);
    dev.init(&mut *rec).unwrap();
    let db = PvDatabase::new();
    db.add_record("A:R", rec).await.unwrap();
    db.ioc_init().await;
    {
        let cell = db.get_record("A:R").unwrap();
        let mut inst = cell.write();
        inst.common.dtyp = dtyp.into();
        inst.common.udf = 0;
        inst.device = Some(dev);
    }
    let _ = db
        .put_record_field_from_ca_no_notify("A:R", field, val.clone())
        .await;
    let failed = settled(&db).await;
    runtime.port_handle().exception_disconnect(0).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while runtime.port_handle().is_connected(0).await.unwrap() {
        assert!(
            std::time::Instant::now() < deadline,
            "the port never disconnected"
        );
        epics_base_rs::runtime::task::sleep(Duration::from_millis(2)).await;
    }
    let _ = db
        .put_record_field_from_ca_no_notify("A:R", field, val)
        .await;
    [failed, settled(&db).await]
}

fn octet_write(port: &'static str, can_block: bool) -> Case {
    Case {
        port,
        can_block,
        dtyp: "asynOctetWrite",
        param: ParamType::Octet,
        rec: Box::new(StringoutRecord::new("")),
        inp: false,
        put: ("VAL", EpicsValue::String("x".into())),
    }
}

fn array_write(port: &'static str, can_block: bool) -> Case {
    let mut aao = WaveformRecord::new(4, DbFieldType::Long);
    aao.kind = ArrayKind::Aao;
    Case {
        port,
        can_block,
        dtyp: "asynInt32ArrayOut",
        param: ParamType::Int32Array,
        rec: Box::new(aao),
        inp: false,
        put: ("VAL", EpicsValue::LongArray(vec![1, 2].into())),
    }
}

/// A failed transfer's alarm, raised again by the refusal.
fn failed_twice(stat: u16) -> [(u16, AlarmSeverity); 2] {
    [(stat, AlarmSeverity::Invalid); 2]
}

#[epics_macros_rs::epics_test]
async fn a_refused_octet_or_array_write_keeps_the_last_alarm() {
    assert_eq!(
        failed_then_refused(octet_write("REF_OCT_BLOCK", true)).await,
        failed_twice(alarm_status::WRITE_ALARM)
    );
    assert_eq!(
        failed_then_refused(array_write("REF_ARR_BLOCK", true)).await,
        failed_twice(alarm_status::READ_ALARM)
    );
}

#[epics_macros_rs::epics_test]
async fn a_refused_octet_or_array_transfer_keeps_the_last_alarm_on_any_port() {
    for can_block in [false, true] {
        let tag = if can_block { "BLOCK" } else { "SYNC" };
        let port = |name: &str| -> &'static str { format!("REF_{name}_{tag}").leak() };
        if !can_block {
            assert_eq!(
                failed_then_refused(octet_write(port("OCT"), false)).await,
                failed_twice(alarm_status::WRITE_ALARM),
                "{tag}"
            );
            assert_eq!(
                failed_then_refused(array_write(port("ARR"), false)).await,
                failed_twice(alarm_status::READ_ALARM),
                "{tag}"
            );
        }
        // C `callbackWfWrite` (devAsynOctet.c:1071-1091).
        assert_eq!(
            failed_then_refused(Case {
                port: port("WFW"),
                can_block,
                dtyp: "asynOctetWrite",
                param: ParamType::Octet,
                rec: Box::new(WaveformRecord::new(8, DbFieldType::Char)),
                inp: true,
                put: ("VAL", EpicsValue::CharArray(b"ab".to_vec().into())),
            })
            .await,
            failed_twice(alarm_status::WRITE_ALARM),
            "{tag}"
        );
        assert_eq!(
            failed_then_refused(Case {
                port: port("SI"),
                can_block,
                dtyp: "asynOctetRead",
                param: ParamType::Octet,
                rec: Box::new(StringinRecord::new("")),
                inp: true,
                put: ("PROC", EpicsValue::Short(1)),
            })
            .await,
            failed_twice(alarm_status::READ_ALARM),
            "{tag}"
        );
        assert_eq!(
            failed_then_refused(Case {
                port: port("WFR"),
                can_block,
                dtyp: "asynInt32ArrayIn",
                param: ParamType::Int32Array,
                rec: Box::new(WaveformRecord::new(4, DbFieldType::Long)),
                inp: true,
                put: ("PROC", EpicsValue::Short(1)),
            })
            .await,
            failed_twice(alarm_status::READ_ALARM),
            "{tag}"
        );
    }
}
