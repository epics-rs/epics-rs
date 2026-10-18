//! A blocking port's refused output `queueRequest`: C `processAo` maps the
//! refusal's status through `asynStatusToEpicsAlarm` with the WRITE_ALARM
//! default and raises it (devAsynInt32.c:1003-1011), so a disconnected port
//! is COMM/INVALID, not WRITE/INVALID.
#![cfg(feature = "epics")]

use std::time::Duration;

use asyn_rs::adapter::{AsynDeviceSupport, AsynLink};
use asyn_rs::param::ParamType;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, ScanType};
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

struct Down(PortDriverBase);

impl PortDriver for Down {
    fn base(&self) -> &PortDriverBase {
        &self.0
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.0
    }
}

#[epics_macros_rs::epics_test]
async fn a_disconnected_blocking_port_raises_comm_on_a_write() {
    let mut base = PortDriverBase::new(
        "REFUSED_OUT",
        1,
        PortFlags {
            can_block: true,
            ..PortFlags::default()
        },
    );
    base.create_param("VAL", ParamType::Int32).unwrap();
    base.set_auto_connect(false);
    base.set_connected(false);
    let (runtime, _actor) =
        create_port_runtime_unclaimed(Down(base), RuntimeConfig::default()).unwrap();
    let mut rec = AoRecord::new(0.0);
    let mut ads = AsynDeviceSupport::from_handle(
        runtime.port_handle().clone(),
        AsynLink {
            port_name: "REFUSED_OUT".into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "VAL".into(),
        },
        "asynInt32",
    );
    ads.set_record_info("A:AO", ScanType::Passive);
    ads.init(&mut rec).unwrap();
    let db = PvDatabase::new();
    db.add_record("A:AO", Box::new(rec)).await.unwrap();
    db.ioc_init().await;
    {
        let cell = db.get_record("A:AO").unwrap();
        let mut inst = cell.write();
        inst.common.dtyp = "asynInt32".into();
        inst.common.udf = 0;
        inst.device = Some(Box::new(ads));
    }
    let _ = db
        .put_record_field_from_ca_no_notify("A:AO", "VAL", EpicsValue::Double(1.0))
        .await;
    let rec = db.get_record("A:AO").unwrap();
    let inst = rec.read();
    assert!(!inst.is_processing(), "a refused queue does not go PACT");
    assert_eq!(
        (inst.common.stat, inst.common.sevr),
        (alarm_status::COMM_ALARM, AlarmSeverity::Invalid)
    );
}
