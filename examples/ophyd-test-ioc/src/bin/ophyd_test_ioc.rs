//! ophyd test IOC binary.
//!
//! Provides all EPICS PVs expected by the ophyd test suite:
//!   - 9 motors (XF:31IDA-OP{Tbl-Ax:X1..X6}Mtr, sim:mtr1..2, FakeMtr)
//!   - 6 sensors (XF:31IDA-BI{Dev:1..6}E-I)
//!   - 1 SimDetector with standard plugins (XF:31IDA-BI{Cam:Tbl}: and ADSIM:)
//!
//! Replaces the Docker-based epics-services-for-ophyd.
//!
//! Usage:
//!   cargo run -p ophyd-test-ioc --features ioc -- ioc/st.cmd
// On `exec_backend` this program's `main` refuses instead of running, so
// everything below is unreachable in that configuration by construction.
// The default build still lints the file in full.
#![cfg_attr(exec_backend, allow(dead_code, unused_imports))]

#[cfg(tokio_backend)]
use ad_plugins_rs::ioc::AdIoc;
use epics_base_rs::error::CaResult;
use motor_rs::ioc::SimMotorHolder;

#[cfg(tokio_backend)]
#[epics_base_rs::epics_main]
async fn main() -> CaResult<()> {
    epics_base_rs::runtime::env::set_default("OPHYD_TEST_IOC", env!("CARGO_MANIFEST_DIR"));

    let mut ioc = AdIoc::new();

    // Motor record type
    let (motor_name, motor_factory) = motor_rs::motor_record_factory();
    ioc.register_record_type(motor_name, motor_factory);

    // SimDetector (simDetectorConfig command + simDetector.template)
    sim_detector::ioc_support::register(&mut ioc);

    // Motors (simMotorCreate command + motor.template)
    epics_base_rs::runtime::env::set_default("MOTOR", motor_rs::MOTOR_IOC_DIR);
    let motor_holder = SimMotorHolder::new();
    ioc.register_startup_command(motor_holder.sim_motor_create_command());
    ioc.register_dynamic_device_support(motor_holder.device_support_factory());

    ioc.run_from_args_with_pva().await
}

/// The `exec_backend` arm: `ad_plugins_rs::ioc` is compiled only on the tokio
/// backend, so there is no IOC to build here.
#[cfg(exec_backend)]
fn main() -> CaResult<()> {
    eprintln!(
        "ophyd_test_ioc needs the tokio backend; this build selects the \
         reactor-free execution model (EPICS_RS_BUILD_EXEC_BACKEND=thread)."
    );
    Ok(())
}
