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
        },
        ArgDesc {
            name: "queueSize",
            arg_type: ArgType::Int,
        },
        ArgDesc {
            name: "blockingCallbacks",
            arg_type: ArgType::Int,
        },
        ArgDesc {
            name: "NDArrayPort",
            arg_type: ArgType::String,
        },
        ArgDesc {
            name: "NDArrayAddr",
            arg_type: ArgType::Int,
        },
        ArgDesc {
            name: "maxBuffers",
            arg_type: ArgType::Int,
        },
        ArgDesc {
            name: "maxMemory",
            arg_type: ArgType::Int,
        },
        ArgDesc {
            name: "priority",
            arg_type: ArgType::Int,
        },
        ArgDesc {
            name: "stackSize",
            arg_type: ArgType::Int,
        },
        ArgDesc {
            name: "maxThreads",
            arg_type: ArgType::Int,
        },
    ]
}

/// Arg descriptors for the two plugin `*Configure` commands whose C signature
/// inserts a count at argument 5, pushing every later argument along by one so
/// that `maxThreads` lands at 10: `maxOverlays` (NDPluginOverlay.cpp:523-526)
/// and `maxROIs` (NDPluginROIStat.cpp:590-593).
///
/// The generic list stops at argument 9, so a C st.cmd line for either of
/// these could not even carry its `maxThreads` without this.
pub fn plugin_arg_defs_with_count(count_name: &'static str) -> Vec<ArgDesc> {
    let mut defs = plugin_arg_defs();
    defs.insert(
        5,
        ArgDesc {
            name: count_name,
            arg_type: ArgType::Int,
        },
    );
    defs
}

/// Arg descriptors for `NDAttrConfigure`, whose C signature inserts
/// `maxAttributes` at index 5 (NDPluginAttribute.cpp:222-242):
/// `(portName, queueSize, blockingCallbacks, NDArrayPort, NDArrayAddr,
/// maxAttributes, maxBuffers, maxMemory, priority, stackSize)` — note there is
/// no `maxThreads` arg, unlike the generic plugin layout.
pub fn attr_arg_defs() -> Vec<ArgDesc> {
    let int = |name| ArgDesc {
        name,
        arg_type: ArgType::Int,
    };
    let string = |name| ArgDesc {
        name,
        arg_type: ArgType::String,
    };
    vec![
        ArgDesc {
            name: "portName",
            arg_type: ArgType::String,
        },
        int("queueSize"),
        int("blockingCallbacks"),
        string("NDArrayPort"),
        int("NDArrayAddr"),
        int("maxAttributes"),
        int("maxBuffers"),
        int("maxMemory"),
        int("priority"),
        int("stackSize"),
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

/// The `maxThreads` argument of a plugin `*Configure` command, as written.
///
/// C's `NDPluginDriver` constructor floors it — `if (maxThreads < 1)
/// maxThreads = 1` (NDPluginDriver.cpp:117) — and an absent iocsh int
/// argument is 0, so 0 is the "operator said nothing" value on both sides.
/// The floor is deliberately not applied here: the plugin's `MAX_THREADS`
/// owner clamps every write, and duplicating the clamp gives two sites that
/// can disagree about the ceiling the plugin is actually running on.
///
/// `index` is the argument's position in *that command's* C signature, which
/// is not uniform. Most carry it at 9; the two whose signature inserts an
/// extra count before `maxBuffers` carry it at 10 (`maxOverlays`,
/// NDPluginOverlay.cpp:523-526; `maxROIs`, NDPluginROIStat.cpp:590-593). A
/// command whose C signature has no `maxThreads` must not call this at all —
/// index 9 there is `stackSize` (NDPluginProcess.cpp:436-439,
/// NDPluginPva.cpp:182-184, NDPluginTimeSeries.cpp:537-541), and reading it
/// would turn a stack size into a thread count.
pub fn max_threads_arg(args: &[ArgValue], index: usize) -> i32 {
    match args.get(index) {
        Some(ArgValue::Int(n)) => *n as i32,
        _ => 0,
    }
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
                },
                ArgDesc {
                    name: "arg2",
                    arg_type: ArgType::String,
                },
            ],
            format!("{name} [args...] - no-op (not implemented)"),
            move |_args: &[ArgValue], _ctx: &CommandContext| {
                // B16: callbackSetQueueSize sizes the asyn callback queue in C.
                // The Rust plugin model uses a per-plugin bounded channel sized
                // at plugin construction, so this is a genuine no-op — warn so
                // a cmd file tuning it is not silently ineffective.
                if name == "callbackSetQueueSize" {
                    eprintln!(
                        "warning: callbackSetQueueSize is a no-op in epics-rs; \
                         plugin queue size is set per-plugin at construction"
                    );
                }
                Ok(CommandOutcome::Continue)
            },
        ));
    }
    app
}
