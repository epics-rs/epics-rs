//! A configure line's `queueSize` below 1 runs, and reads back, as 1.
//!
//! C publishes the argument (NDPluginDriver.cpp:157-158) and then
//! `createCallbackThreads` floors it, says so, and republishes both params
//! (:989-995, :1024). An absent iocsh int reaches C as 0, so an omitted
//! `queueSize` takes the same floor rather than a default of its own.
// `ad_plugins_rs::ioc` mounts on the tokio-backend-only IOC runners, so
// this file is gated with the module it exercises.
#![cfg(tokio_backend)]
#![cfg(feature = "ioc")]

use std::sync::Arc;

use ad_core_rs::ioc::GenericDriverContext;
use ad_core_rs::ndarray_pool::DriverPool;
use ad_core_rs::plugin::channel::NDArrayOutput;
use ad_plugins_rs::ioc::AdIoc;
use asyn_rs::port::DrvUserRequest;
use asyn_rs::port_handle::PortHandle;
use asyn_rs::services::PortServices;
use asyn_rs::trace::TraceFile;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::CommandOutcome;

fn ad_ioc_with_driver(upstream: &str) -> (AdIoc, IocShell) {
    let ioc = AdIoc::new();
    let pool = DriverPool::standalone(4_000_000);
    let output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
    ioc.mgr().set_driver(Arc::new(GenericDriverContext::new(
        pool,
        output,
        upstream,
        ioc.mgr().wiring(),
    )));

    let rt = tokio::runtime::Runtime::new().unwrap();
    let db = Arc::new(PvDatabase::new());
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    // The shell's command handlers block on this runtime; it must outlive them.
    std::mem::forget(rt);

    let shell = IocShell::new(db, bridge);
    for def in ioc.app().startup_commands() {
        shell.register(def.clone());
    }
    (ioc, shell)
}

fn run(shell: &IocShell, line: &str) {
    match shell.execute_line(line) {
        Ok(CommandOutcome::Continue) => {}
        Ok(_) => panic!("st.cmd line `{line}` did not continue"),
        Err(e) => panic!("st.cmd line `{line}` failed: {e}"),
    }
}

fn read(port: &PortHandle, drv_info: &str) -> i32 {
    let reason = port
        .drv_user_create_blocking(&DrvUserRequest::new(drv_info, 0))
        .unwrap_or_else(|e| panic!("{drv_info} does not resolve: {e}"))
        .reason;
    port.read_int32_blocking(reason, 0).unwrap()
}

/// Configure `port` with `line` and return its QueueSize, QueueFree and
/// what its trace printed while it was built.
fn configure(shell: &IocShell, port: &str, line: &str) -> (i32, i32, String) {
    let log = std::env::temp_dir().join(format!("qsz-{port}-{}.log", std::process::id()));
    let file = std::fs::File::create(&log).unwrap();
    // The slot is keyed by name, so it is in place before the port exists.
    PortServices::global().trace().set_trace_file(
        Some(port),
        TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
    );
    run(shell, line);
    let handle = asyn_rs::registry::get_port(port)
        .unwrap_or_else(|| panic!("{port} was never registered"))
        .handle;
    let trace = std::fs::read_to_string(&log).unwrap();
    let _ = std::fs::remove_file(&log);
    (
        read(&handle, "QUEUE_SIZE"),
        read(&handle, "QUEUE_FREE"),
        trace,
    )
}

const FLOOR: &str = "NDPluginDriver::createCallbackThreads error, queueSize=";

#[test]
fn queue_size_below_one_is_floored_with_c_s_line() {
    let (_ioc, shell) = ad_ioc_with_driver("QSUP1");
    for (port, arg, written) in [("QSZ0", "0", 0), ("QSZN", "-3", -3)] {
        let (size, free, trace) = configure(
            &shell,
            port,
            &format!(r#"NDStatsConfigure("{port}", {arg}, 0, "QSUP1", 0)"#),
        );
        assert_eq!((size, free), (1, 1), "{port}: QueueSize/QueueFree");
        let line = format!("{FLOOR}{written} must be >= 1, setting to 1");
        assert!(
            trace.contains(&line),
            "{port}: missing {line:?} in\n{trace}"
        );
    }
}

#[test]
fn queue_size_of_one_is_taken_as_written() {
    let (_ioc, shell) = ad_ioc_with_driver("QSUP2");
    let (size, free, trace) = configure(
        &shell,
        "QSZ1",
        r#"NDStatsConfigure("QSZ1", 1, 0, "QSUP2", 0)"#,
    );
    assert_eq!((size, free), (1, 1));
    assert!(!trace.contains(FLOOR), "a queue of 1 was refused:\n{trace}");
}

/// Both parse sites: the generic layout and `NDAttrPlotConfig`, which carries
/// `queueSize` at argument 6 (NDPluginAttrPlot.cpp:348-350).
#[test]
fn an_omitted_queue_size_is_iocsh_s_zero() {
    let (_ioc, shell) = ad_ioc_with_driver("QSUP3");
    for (port, line) in [
        ("QSZA", r#"NDStatsConfigure("QSZA")"#.to_string()),
        (
            "QSZP",
            r#"NDAttrPlotConfig("QSZP", 8, 1000, 4, "QSUP3", 0)"#.to_string(),
        ),
    ] {
        let (size, free, trace) = configure(&shell, port, &line);
        assert_eq!((size, free), (1, 1), "{port}: QueueSize/QueueFree");
        let floor = format!("{FLOOR}0 must be >= 1, setting to 1");
        assert!(
            trace.contains(&floor),
            "{port}: missing {floor:?} in\n{trace}"
        );
    }
}

/// With blocking callbacks C's constructor skips `createCallbackThreads`
/// (NDPluginDriver.cpp:164-166), so the queueSize stands as written until the
/// first BlockingCallbacks 0 runs it (:715-717). That write succeeds: its
/// status is dropped, and only the floor's own line prints.
#[test]
fn blocking_callbacks_defer_the_floor_to_the_first_non_blocking_write() {
    let (_ioc, shell) = ad_ioc_with_driver("QSUP4");
    let log = std::env::temp_dir().join(format!("qsz-QSZB-{}.log", std::process::id()));
    let file = Arc::new(std::sync::Mutex::new(std::fs::File::create(&log).unwrap()));
    let trace_mgr = Arc::clone(PortServices::global().trace());
    trace_mgr.set_trace_file(Some("QSZB"), TraceFile::File(Arc::clone(&file)));
    run(&shell, r#"NDStatsConfigure("QSZB", 0, 1, "QSUP4", 0)"#);
    let port = asyn_rs::registry::get_port("QSZB").unwrap().handle;
    // `pasynUserSelf` prints through device 0 (asynPortDriver.cpp:4099),
    // which on this multi-address port keeps a trace file of its own.
    trace_mgr.set_device_trace_file("QSZB", 0, TraceFile::File(file));
    let trace = || std::fs::read_to_string(&log).unwrap();

    assert_eq!(
        (read(&port, "QUEUE_SIZE"), read(&port, "QUEUE_FREE")),
        (0, 0),
        "C publishes the argument unfloored (:157-158)"
    );
    assert!(
        !trace().contains(FLOOR),
        "floored while blocking:\n{}",
        trace()
    );

    let blocking = port
        .drv_user_create_blocking(&DrvUserRequest::new("BLOCKING_CALLBACKS", 0))
        .unwrap()
        .reason;
    port.write_int32_blocking(blocking, 0, 0)
        .expect("the createCallbackThreads status is dropped (:715-717)");
    assert_eq!(
        (read(&port, "QUEUE_SIZE"), read(&port, "QUEUE_FREE")),
        (1, 1)
    );
    let line = format!("{FLOOR}0 must be >= 1, setting to 1");
    assert_eq!(trace().matches(&line).count(), 1, "in\n{}", trace());
    assert!(
        !trace().contains("writeInt32 ERROR"),
        "the write failed:\n{}",
        trace()
    );

    // The threads exist now; going blocking and back does not rerun it.
    port.write_int32_blocking(blocking, 0, 1).unwrap();
    port.write_int32_blocking(blocking, 0, 0).unwrap();
    assert_eq!(trace().matches(&line).count(), 1, "in\n{}", trace());
    let _ = std::fs::remove_file(&log);
}
