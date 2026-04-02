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
        let trace = ioc.trace().clone();
        let rt = driver_runtime.clone();
        ioc.register_startup_command(CommandDef::new(
            "simDetectorConfig",
            vec![
                ArgDesc { name: "portName", arg_type: ArgType::String, optional: false },
                ArgDesc { name: "sizeX", arg_type: ArgType::Int, optional: true },
                ArgDesc { name: "sizeY", arg_type: ArgType::Int, optional: true },
                ArgDesc { name: "maxMemory", arg_type: ArgType::Int, optional: true },
            ],
            "simDetectorConfig portName [sizeX] [sizeY] [maxMemory]",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let port_name = match &args[0] {
                    ArgValue::String(s) => s.clone(),
                    _ => return Err("portName required".into()),
                };
                let size_x = match args.get(1) { Some(ArgValue::Int(n)) => *n as i32, _ => 256 };
                let size_y = match args.get(2) { Some(ArgValue::Int(n)) => *n as i32, _ => 256 };
                let max_memory = match args.get(3) { Some(ArgValue::Int(n)) => *n as usize, _ => 50_000_000 };

                println!("simDetectorConfig: port={port_name}, size={size_x}x{size_y}, maxMemory={max_memory}");

                let runtime = create_sim_detector(&port_name, size_x, size_y, max_memory, NDArrayOutput::new())
                    .map_err(|e| format!("failed to create SimDetector: {e}"))?;

                let port_handle = runtime.port_handle().clone();

                asyn_rs::asyn_record::register_port(&port_name, port_handle, trace.clone());

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
