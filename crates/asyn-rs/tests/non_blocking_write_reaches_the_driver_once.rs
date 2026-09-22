//! One put on an output record bound to a port without `ASYN_CANBLOCK`
//! reaches the driver's write once. `write_begin` completed the write on
//! such a port and returned the "fall back to `write()`" answer, so the
//! framework wrote the same value a second time: a USB-CTR pulse generator
//! restarted twice per put, and `MCA_START_ACQUIRE` on a finished run
//! delivered `MCA_ACQUIRING` 1,0,1,0 to an I/O Intr record.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use asyn_rs::adapter::{AsynDeviceSupport, AsynLink};
use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::record::ScanType;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

struct CountingPort {
    base: PortDriverBase,
    writes: Arc<AtomicUsize>,
}

impl PortDriver for CountingPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.base.params.set_float64(user.reason, user.addr, value)
    }
}

#[epics_macros_rs::epics_test]
async fn a_put_on_a_non_blocking_port_writes_the_driver_once() {
    const PORT: &str = "NBONCE";
    let writes = Arc::new(AtomicUsize::new(0));
    let mut base = PortDriverBase::new(
        PORT,
        1,
        PortFlags {
            can_block: false,
            ..PortFlags::default()
        },
    );
    base.create_param("VAL", ParamType::Float64).unwrap();
    let (runtime, _actor) = create_port_runtime(
        CountingPort {
            base,
            writes: writes.clone(),
        },
        RuntimeConfig::default(),
    )
    .unwrap();
    assert!(!runtime.port_handle().can_block());

    let mut rec = AoRecord::new(0.0);
    let mut ads = AsynDeviceSupport::from_handle(
        runtime.port_handle().clone(),
        AsynLink {
            port_name: PORT.into(),
            addr: 0,
            timeout: Some(Duration::from_secs(1)),
            drv_info: "VAL".into(),
        },
        "asynFloat64",
    );
    ads.set_record_info("NB:AO", ScanType::Passive);
    ads.init(&mut rec).unwrap();

    let db = PvDatabase::new();
    db.add_record("NB:AO", Box::new(rec)).await.unwrap();
    {
        let cell = db.get_record("NB:AO").unwrap();
        let mut inst = cell.write();
        inst.common.dtyp = "asynFloat64".into();
        inst.common.udf = 0;
        inst.device = Some(Box::new(ads));
    }

    db.put_record_field_from_ca_no_notify("NB:AO", "VAL", EpicsValue::Double(2.5))
        .await
        .unwrap();
    for _ in 0..500 {
        if !db.get_record("NB:AO").unwrap().read().is_processing() {
            assert_eq!(
                writes.load(Ordering::SeqCst),
                1,
                "one put, one driver write"
            );
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("the cycle never ended");
}
