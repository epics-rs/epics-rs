use epics_base_rs::server::iocsh::registry::*;
use epics_ca_rs::server::ioc_app::IocApplication;

/// Standard C-compatible arg descriptors for plugin configure commands.
///
/// Matches: `(portName, queueSize, blockingCallbacks, NDArrayPort, NDArrayAddr, maxBuffers, maxMemory, priority, stackSize, maxThreads)`
pub fn plugin_arg_defs() -> Vec<ArgDesc> {
    vec![
        ArgDesc {
            name: "portName",
            arg_type: ArgType::String,
            optional: false,
        },
        ArgDesc {
            name: "queueSize",
            arg_type: ArgType::Int,
            optional: true,
        },
        ArgDesc {
            name: "blockingCallbacks",
            arg_type: ArgType::Int,
            optional: true,
        },
        ArgDesc {
            name: "NDArrayPort",
            arg_type: ArgType::String,
            optional: true,
        },
        ArgDesc {
            name: "NDArrayAddr",
            arg_type: ArgType::Int,
            optional: true,
        },
        ArgDesc {
            name: "maxBuffers",
            arg_type: ArgType::Int,
            optional: true,
        },
        ArgDesc {
            name: "maxMemory",
            arg_type: ArgType::Int,
            optional: true,
        },
        ArgDesc {
            name: "priority",
            arg_type: ArgType::Int,
            optional: true,
        },
        ArgDesc {
            name: "stackSize",
            arg_type: ArgType::Int,
            optional: true,
        },
        ArgDesc {
            name: "maxThreads",
            arg_type: ArgType::Int,
            optional: true,
        },
    ]
}

/// Extract port_name, queue_size, and ndarray_port from C-compatible plugin args.
///
/// C format: `(portName, queueSize, blockingCallbacks, NDArrayPort, NDArrayAddr, ...)`
pub fn extract_plugin_args(args: &[ArgValue]) -> Result<(String, usize, String), String> {
    let port_name = match &args[0] {
        ArgValue::String(s) => s.clone(),
        _ => return Err("portName required".into()),
    };
    let queue_size = match args.get(1) {
        Some(ArgValue::Int(n)) => *n as usize,
        _ => 20,
    };
    let ndarray_port = match args.get(3) {
        Some(ArgValue::String(s)) => s.clone(),
        _ => String::new(),
    };
    Ok((port_name, queue_size, ndarray_port))
}

/// Auto-derive DTYP name from port name: `"ROI1"` → `"asynROI1"`.
pub fn dtyp_from_port(port_name: &str) -> String {
    format!("asyn{port_name}")
}

/// Register no-op commands commonly found in C commonPlugins.cmd scripts.
///
/// These are silently ignored but must be registered so the iocsh parser
/// doesn't error on them.
///
/// Note: autosave commands (`set_requestfile_path`, `set_savefile_path`,
/// `set_pass0_restoreFile`, `set_pass1_restoreFile`, `save_restoreSet_status_prefix`,
/// `create_monitor_set`, `create_triggered_set`) are no longer registered here.
/// They are handled by `AutosaveStartupConfig::register_startup_commands()`.
pub fn register_noop_commands(mut app: IocApplication) -> IocApplication {
    for cmd in &["startPVAServer", "callbackSetQueueSize"] {
        let name = *cmd;
        app = app.register_startup_command(CommandDef::new(
            name,
            vec![
                ArgDesc {
                    name: "arg1",
                    arg_type: ArgType::String,
                    optional: true,
                },
                ArgDesc {
                    name: "arg2",
                    arg_type: ArgType::String,
                    optional: true,
                },
            ],
            format!("{name} [args...] - no-op (not implemented)"),
            move |_args: &[ArgValue], _ctx: &CommandContext| Ok(CommandOutcome::Continue),
        ));
    }
    app
}
