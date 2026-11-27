//! A plugin's array waveforms are `I/O Intr` on the interfaces its C
//! constructor adds to `NDPluginDriver`'s interrupt mask. With the port
//! refusing clients outside that mask (`getInterruptPvt`,
//! asynManager.c:2492-2497), a plugin that does not carry its C mask over
//! leaves those records never processing.
// `ad_plugins_rs::ioc` mounts on the tokio-backend-only IOC runners, so
// this file is gated with the module it exercises.
#![cfg(tokio_backend)]
#![cfg(feature = "ioc")]

use std::sync::Arc;

use ad_core_rs::ioc::GenericDriverContext;
use ad_core_rs::ndarray_pool::DriverPool;
use ad_core_rs::plugin::channel::NDArrayOutput;
use ad_plugins_rs::ioc::AdIoc;
use asyn_rs::interfaces::InterfaceType;
use asyn_rs::interrupt::InterruptFilter;
use asyn_rs::port_handle::PortHandle;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::CommandOutcome;

/// An `AdIoc` with a detector configured, whose output the plugins read.
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
        Ok(CommandOutcome::Failed) => panic!("st.cmd line `{line}` failed"),
        Ok(CommandOutcome::Exit) => panic!("st.cmd line `{line}` ended the shell"),
        Err(e) => panic!("st.cmd line `{line}` failed: {e}"),
    }
}

fn plugin_port(name: &str) -> PortHandle {
    asyn_rs::registry::get_port(name)
        .unwrap_or_else(|| panic!("{name} was never registered"))
        .handle
}

/// Configure one plugin and register an `I/O Intr` client on each of `ifaces`
/// at its port, as devAsyn's `getIoIntInfo` does.
fn assert_interrupt_sources(upstream: &str, line: &str, name: &str, ifaces: &[InterfaceType]) {
    let (_ioc, shell) = ad_ioc_with_driver(upstream);
    run(&shell, line);
    let port = plugin_port(name);
    let refused: Vec<_> = ifaces
        .iter()
        .filter_map(|&iface| {
            port.interrupts()
                .register_interrupt_user(
                    InterruptFilter {
                        iface: Some(iface),
                        ..Default::default()
                    },
                    0,
                )
                .err()
                .map(|e| e.message_display().to_string())
        })
        .collect();
    assert!(refused.is_empty(), "{name}: {refused:#?}");
}

/// NDPluginStats.cpp:778-779.
#[test]
fn stats_float64_arrays_interrupt() {
    assert_interrupt_sources(
        "ISUP1",
        r#"NDStatsConfigure("ISST1", 20, 0, "ISUP1", 0)"#,
        "ISST1",
        &[InterfaceType::Float64Array],
    );
}

/// NDPluginROIStat.cpp:496-497.
#[test]
fn roi_stat_float64_arrays_interrupt() {
    assert_interrupt_sources(
        "ISUP2",
        r#"NDROIStatConfigure("ISRS1", 20, 0, "ISUP2", 0, 8)"#,
        "ISRS1",
        &[InterfaceType::Float64Array],
    );
}

/// NDPluginFFT.cpp:51-52.
#[test]
fn fft_float64_arrays_interrupt() {
    assert_interrupt_sources(
        "ISUP3",
        r#"NDFFTConfigure("ISFF1", 20, 0, "ISUP3", 0)"#,
        "ISFF1",
        &[InterfaceType::Float64Array],
    );
}

/// NDPluginAttribute.cpp:176-177.
#[test]
fn attribute_float64_arrays_interrupt() {
    assert_interrupt_sources(
        "ISUP4",
        r#"NDAttrConfigure("ISAT1", 20, 0, "ISUP4", 0, 8)"#,
        "ISAT1",
        &[InterfaceType::Float64Array],
    );
}

/// NDPluginStdArrays.cpp:324-328.
#[test]
fn std_arrays_every_array_type_interrupts() {
    assert_interrupt_sources(
        "ISUP5",
        r#"NDStdArraysConfigure("ISSA1", 20, 0, "ISUP5", 0)"#,
        "ISSA1",
        &[
            InterfaceType::Int8Array,
            InterfaceType::Int16Array,
            InterfaceType::Int32Array,
            InterfaceType::Int64Array,
            InterfaceType::Float32Array,
            InterfaceType::Float64Array,
        ],
    );
}

/// NDPluginTimeSeries.cpp:52-53.
#[test]
fn time_series_float64_arrays_interrupt() {
    assert_interrupt_sources(
        "ISUP6",
        r#"NDTimeSeriesConfigure("ISTS1", 20, 0, "ISUP6", 0, 8)"#,
        "ISTS1",
        &[InterfaceType::Float64Array],
    );
}

/// NDPluginAttrPlot.cpp:51-55.
#[test]
fn attr_plot_float64_arrays_interrupt() {
    assert_interrupt_sources(
        "ISUP7",
        r#"NDAttrPlotConfig("ISAP1", 8, 100, 4, "ISUP7", 0, 20, 0)"#,
        "ISAP1",
        &[InterfaceType::Float64Array],
    );
}

/// NDPluginCircularBuff.cpp:400-401.
#[test]
fn circular_buff_float64_arrays_interrupt() {
    assert_interrupt_sources(
        "ISUP8",
        r#"NDCircularBuffConfigure("ISCB1", 20, 0, "ISUP8", 0)"#,
        "ISCB1",
        &[InterfaceType::Float64Array],
    );
}

/// NDPluginProcess.cpp:359-360.
#[test]
fn process_float64_arrays_interrupt() {
    assert_interrupt_sources(
        "ISUP9",
        r#"NDProcessConfigure("ISPR1", 20, 0, "ISUP9", 0)"#,
        "ISPR1",
        &[InterfaceType::Float64Array],
    );
}

/// NDPluginROI.cpp:313-314.
#[test]
fn roi_float64_arrays_interrupt() {
    assert_interrupt_sources(
        "ISUP10",
        r#"NDROIConfigure("ISRO1", 20, 0, "ISUP10", 0)"#,
        "ISRO1",
        &[InterfaceType::Float64Array],
    );
}

/// NDPluginScatter.cpp:117-118.
#[test]
fn scatter_float64_arrays_interrupt() {
    assert_interrupt_sources(
        "ISUP11",
        r#"NDScatterConfigure("ISSC1", 20, 0, "ISUP11", 0)"#,
        "ISSC1",
        &[InterfaceType::Float64Array],
    );
}

/// NDPluginTransform.cpp:594-595.
#[test]
fn transform_float64_arrays_interrupt() {
    assert_interrupt_sources(
        "ISUP12",
        r#"NDTransformConfigure("ISTR1", 20, 0, "ISUP12", 0)"#,
        "ISTR1",
        &[InterfaceType::Float64Array],
    );
}

/// NDPosPlugin.cpp:372-373.
#[test]
fn pos_plugin_float64_arrays_interrupt() {
    assert_interrupt_sources(
        "ISUP13",
        r#"NDPosPluginConfigure("ISPO1", 20, 0, "ISUP13", 0)"#,
        "ISPO1",
        &[InterfaceType::Float64Array],
    );
}
