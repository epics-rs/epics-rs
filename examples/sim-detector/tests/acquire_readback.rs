//! Record-level reproduction for the AD `Acquire` bo residual: after a
//! Single-mode acquire completes, the `asyn:READBACK` Acquire command bo
//! must return to VAL=0 (Done), mirroring the driver-side ACQUIRE param.
//!
//! Driver-level parity is already covered by `integration.rs`
//! (`test_single_mode_one_frame` asserts the ACQUIRE *param* → 0). This
//! test wires a real `bo` record + `asyn:READBACK` device support on top of
//! the real SimDetector driver and acquisition task, and replicates the
//! `setup_io_intr` consumer, so it exercises the full record-callback path.
#![cfg(feature = "ioc")]

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use ad_core_rs::driver::ImageMode;
use ad_core_rs::plugin::channel::NDArrayOutput;
use sim_detector::create_sim_detector;

use asyn_rs::adapter::{AsynDeviceSupport, AsynLink};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::record::ScanType;
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::types::EpicsValue;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acquire_bo_returns_to_zero_after_single() {
    let rt = create_sim_detector("RBK_TEST", 32, 32, 10_000_000, NDArrayOutput::new()).unwrap();
    let handle = rt.port_handle().clone();

    // Single mode, short exposure.
    handle
        .write_int32(rt.ad_params.image_mode, 0, ImageMode::Single as i32)
        .await
        .unwrap();
    handle
        .write_float64(rt.ad_params.acquire_time, 0, 0.001)
        .await
        .unwrap();
    handle
        .write_float64(rt.ad_params.acquire_period, 0, 0.001)
        .await
        .unwrap();

    // Build the Acquire command bo wired to the driver's ACQUIRE param
    // via asyn:READBACK.
    let db = Arc::new(PvDatabase::new());
    db.add_record("Acquire", Box::new(BoRecord::new(0)))
        .await
        .unwrap();
    let link = AsynLink {
        port_name: "RBK_TEST".into(),
        addr: 0,
        timeout: Duration::from_secs(1),
        drv_info: "ACQUIRE".into(),
    };
    let mut ads = AsynDeviceSupport::from_handle(handle.clone(), link, "asynInt32");
    ads.set_record_info("Acquire", ScanType::Passive);
    ads.set_asyn_readback(true);
    {
        let rec = db.get_record("Acquire").unwrap();
        let mut inst = rec.write();
        inst.common.dtyp = "asynInt32".into();
        ads.init(&mut *inst.record).unwrap();
        inst.device = Some(Box::new(ads));
    }

    // Replicate setup_io_intr: take the device, grab the receiver, put it
    // back, and spawn the consumer that drives process_record_readback.
    let rec_arc = db.get_record("Acquire").unwrap();
    let intr_rx = {
        let mut inst = rec_arc.write();
        let mut dev = inst.device.take().unwrap();
        let r = dev.io_intr_receiver();
        inst.device = Some(dev);
        r
    };
    let mut intr_rx = intr_rx.expect("asyn:READBACK receiver");
    let db2 = db.clone();
    tokio::spawn(async move {
        while intr_rx.recv().await.is_some() {
            let mut visited = HashSet::new();
            let _ = db2
                .process_record_readback("Acquire", &mut visited, 0)
                .await;
        }
    });

    // caput Acquire=1 — a real bo PUT (OUT stage dev.write() starts the
    // acquisition; the in-actor ACQUIRE=1 callback fires the readback).
    {
        let rec = db.get_record("Acquire").unwrap();
        let mut inst = rec.write();
        inst.record.set_val(EpicsValue::Enum(1)).unwrap();
    }
    {
        let mut visited = HashSet::new();
        db.process_record_with_links("Acquire", &mut visited, 0)
            .await
            .unwrap();
    }

    // Wait for the single acquisition + finalize.
    let mut val = None;
    for _ in 0..1000 {
        tokio::time::sleep(Duration::from_millis(2)).await;
        {
            let inst = rec_arc.read();
            val = inst.record.get_field("VAL");
        }
        if val == Some(EpicsValue::Enum(0)) {
            break;
        }
    }

    let drv = handle.read_int32(rt.ad_params.acquire, 0).await.unwrap();
    assert_eq!(drv, 0, "driver ACQUIRE param must return to 0");
    assert_eq!(
        val,
        Some(EpicsValue::Enum(0)),
        "Acquire bo VAL must read the finalize 0 back (Done), got {val:?}"
    );
}
