use std::sync::Arc;

use epics_base_rs::server::iocsh::registry::*;

use crate::driver::{SimDetectorRuntime, create_sim_detector};
use ad_core_rs::ioc::GenericDriverContext;
use ad_core_rs::plugin::channel::NDArrayOutput;

/// Register the SimDetector configure command on an `AdIoc`.
///
/// After calling this, `simDetectorConfig(...)` can be used in st.cmd to
/// create a SimDetector. All records use standard asyn DTYPs handled by
/// the universal asyn device support factory.
pub fn register(ioc: &mut ad_plugins_rs::ioc::AdIoc) {
    epics_base_rs::runtime::env::set_default("ADSIMDETECTOR", env!("CARGO_MANIFEST_DIR"));

    let driver_runtime: Arc<std::sync::Mutex<Option<SimDetectorRuntime>>> =
        Arc::new(std::sync::Mutex::new(None));

    {
        let mgr = ioc.mgr().clone();
        let rt = driver_runtime.clone();
        ioc.register_startup_command(CommandDef::new(
            "simDetectorConfig",
            // simDetector.cpp:1171-1187. maxBuffers is accepted and unused, as
            // C's NDArrayPool takes only maxMemory (asynNDArrayDriver.cpp:881);
            // priority and stackSize only matter to an ASYN_CANBLOCK port,
            // which simDetector is not.
            vec![
                ArgDesc { name: "portName", arg_type: ArgType::String },
                ArgDesc { name: "maxSizeX", arg_type: ArgType::Int },
                ArgDesc { name: "maxSizeY", arg_type: ArgType::Int },
                ArgDesc { name: "dataType", arg_type: ArgType::Int },
                ArgDesc { name: "maxBuffers", arg_type: ArgType::Int },
                ArgDesc { name: "maxMemory", arg_type: ArgType::Int },
                ArgDesc { name: "priority", arg_type: ArgType::Int },
                ArgDesc { name: "stackSize", arg_type: ArgType::Int },
            ],
            "simDetectorConfig portName maxSizeX maxSizeY dataType maxBuffers maxMemory priority stackSize",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let port_name = match &args[0] {
                    ArgValue::String(s) => s.to_str()?.to_owned(),
                    _ => return Err("portName required".into()),
                };
                // An absent iocsh int is 0; C floors a negative maxMemory to 0
                // (unlimited, :1163-1165).
                let int = |i: usize| match args.get(i) {
                    Some(ArgValue::Int(n)) => *n,
                    _ => 0,
                };
                let (size_x, size_y, data_type) = (int(1) as i32, int(2) as i32, int(3) as i32);
                let max_memory = int(5).max(0) as usize;

                // ADDriver is an asynPortDriver: the name is registered before
                // the detector or its acquisition task exist.
                let claim = asyn_rs::manager::claim_port_driver(&port_name).map_err(|e| e.to_string())?;
                let runtime = create_sim_detector(&claim, size_x, size_y, data_type, max_memory, NDArrayOutput::new())
                    .map_err(|e| format!("failed to create SimDetector: {e}"))?;

                let port_handle = runtime.port_handle().clone();

                claim
                    .publish(port_handle, ctx.db().scan_control().interrupt_gate())
                    .map_err(|e| e.to_string())?;

                mgr.set_driver(Arc::new(GenericDriverContext::new(
                    runtime.pool().clone(),
                    runtime.array_output().clone(),
                    &port_name,
                    mgr.wiring(),
                )));

                *rt.lock().unwrap() = Some(runtime);

                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // Keep the runtime alive for the IOC's lifetime.
    ioc.keep_alive(driver_runtime);
}
