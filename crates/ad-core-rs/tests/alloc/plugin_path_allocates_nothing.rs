//! A frame published into a plugin chain allocates nothing once the chain is
//! warm, as C's does: the pool hands back a freed array
//! (NDArrayPool.cpp:161-183), `trySend` copies a pointer into the
//! preallocated `epicsMessageQueue` (NDPluginDriver.cpp:423, :985), and the
//! `set*Param` / `callParamCallbacks` of `begin`/`endProcessCallbacks`
//! write the parameter library in place.
//!
//! The driver publishes through an [`ArrayPublisher`] into a plugin that
//! hands its input on, copied into a free buffer as C's
//! `endProcessCallbacks` copies it (NDPluginDriver.cpp:285), to a plugin
//! that only counts. Everything the publish sets off is counted, on
//! every thread but libtest's; only the driver's own fill of the frame is
//! not. A frame with driver attributes allocates nothing either, where C's
//! copy news each one (NDArrayPool.cpp:301, NDAttribute.cpp:113). A
//! plugin with its own attributes file evaluates the set under its port
//! lent to its thread, and that lend is all it allocates.
//!
//! Each frame waits for both ports to apply its posts, as C's plugin thread
//! writes them under the port lock before it takes the next array. A port
//! actor trailing deeper than it ever has grows the storage it then reuses
//! (asyn's `a_backlog_of_sections_allocates_nothing_once_one_came_back`).

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::task::{Context, Waker};
use std::time::Duration;

use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
use ad_core_rs::ndarray::{NDArray, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::{DriverPool, NDArrayPool, PublishedArray};
use ad_core_rs::plugin::channel::{ArrayPublisher, NDArrayOutput};
use ad_core_rs::plugin::runtime::{
    ASYN_CANBLOCK, DEFAULT_PLUGIN_THREADS, NDPluginProcess, PluginRuntimeHandle, ProcessResult,
    create_plugin_runtime, wire_downstream,
};
use ad_core_rs::plugin::wiring::WiringRegistry;
use alloc_count::exempt;
use alloc_count::named::{self, Name};
use asyn_rs::interrupt::InterruptFilter;
use asyn_rs::param::ParamValue;
use asyn_rs::port::{InterfaceMask, PortFlags};
use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;
use epics_libcom_rs::runtime::task::test_block_on;

/// libtest's `main`, which only waits for the test's thread, apart from
/// every other thread.
static THREADS: [Name; 2] = [Name::Exact("main"), Name::Any];

/// Hands its input on, as NDPluginStats does (NDPluginStats.cpp:651).
struct Forward;

impl NDPluginProcess for Forward {
    const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::NONE;
    const INTERRUPT_MASK: InterfaceMask = InterfaceMask::NONE;
    fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        ProcessResult::forward(array, Vec::new())
    }
    fn plugin_type(&self) -> &str {
        "Forward"
    }
}

/// Counts what reaches it, and what of that carries the plugin file's `Cnt`.
struct Sink(Arc<AtomicUsize>, Arc<AtomicUsize>);

impl NDPluginProcess for Sink {
    const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::NONE;
    const INTERRUPT_MASK: InterfaceMask = InterfaceMask::NONE;
    fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        if array.attributes.get("Cnt").is_some() {
            self.1.fetch_add(1, Ordering::Relaxed);
        }
        self.0.fetch_add(1, Ordering::Release);
        ProcessResult::sink(Vec::new())
    }
    fn plugin_type(&self) -> &str {
        "Sink"
    }
}

fn enable(plugin: &PluginRuntimeHandle) {
    plugin
        .port_runtime()
        .port_handle()
        .write_int32_blocking(plugin.plugin_params.enable_callbacks, 0, 1)
        .unwrap();
    assert!(plugin.wait_params_applied(Duration::from_secs(10)));
}

/// Every frame handled and every post the plugins queued applied: the data
/// loops ack a barrier only with their queues drained, and a read queues
/// behind the posts on each port.
fn quiesce(plugins: &[&PluginRuntimeHandle]) {
    exempt(|| {
        for plugin in plugins {
            assert!(plugin.wait_params_applied(Duration::from_secs(10)));
            plugin
                .port_runtime()
                .port_handle()
                .read_int32_blocking(plugin.ndarray_params.array_counter, 0)
                .unwrap();
        }
    });
}

/// The `ArrayCounter` a plugin's port has last applied.
fn applied(plugin: &PluginRuntimeHandle) -> Arc<AtomicI32> {
    let counter = Arc::new(AtomicI32::new(0));
    let seen = counter.clone();
    let sub = plugin
        .port_runtime()
        .port_handle()
        .interrupts()
        .register_sync_callback(
            InterruptFilter {
                reason: Some(plugin.ndarray_params.array_counter),
                addr: Some(0),
                ..Default::default()
            },
            move |v| {
                if let ParamValue::Int32(n) = v.value {
                    seen.store(n, Ordering::Release);
                }
            },
        )
        .unwrap();
    std::mem::forget(sub);
    counter
}

/// A non-blocking publish is a `trySend`: it completes on its first poll.
fn publish(publisher: &ArrayPublisher, frame: PublishedArray) {
    let mut fut = std::pin::pin!(publisher.publish(frame));
    let polled = fut.as_mut().poll(&mut Context::from_waker(Waker::noop()));
    assert!(polled.is_ready(), "a non-blocking publish waited");
}

const WARM: usize = 64;
const FRAMES: usize = 512;

/// What a driver's `getAttributes` puts on each frame (simDetector.cpp:828).
const DRIVER_ATTRIBUTES: [&str; 4] = ["ColorMode", "BayerPattern", "Gain", "Exposure"];

/// The forwarding plugin's own `NDAttributesFile`, which its
/// `endProcessCallbacks` evaluates onto each copy (NDPluginDriver.cpp:285).
const PLUGIN_ATTRIBUTES: &str = r#"<Attributes>
    <Attribute name="Cnt" type="PARAM" source="ARRAY_COUNTER" datatype="INT"/>
    <Attribute name="Tag" type="CONST" source="abc" datatype="STRING"/>
</Attributes>"#;

/// A lend of a port to a caller's thread: its cancel token, its reply
/// channel and the no-op request the actor parks on.
const LEND: u64 = 3;

#[test]
fn a_steady_frame_through_two_plugins_allocates_nothing() {
    let bare = steady_allocs(["ZALLOC_FWD", "ZALLOC_SINK"], &[], None);
    assert_eq!(bare, 0, "{FRAMES} bare frames after {WARM} warm-up ones");
    let attributed = steady_allocs(["ZALLOC_AFWD", "ZALLOC_ASINK"], &DRIVER_ATTRIBUTES, None);
    assert_eq!(
        attributed, 0,
        "{FRAMES} frames with driver attributes after {WARM} warm-up ones"
    );
    let filed = steady_allocs(
        ["ZALLOC_FFWD", "ZALLOC_FSINK"],
        &[],
        Some(PLUGIN_ATTRIBUTES),
    );
    assert!(
        filed <= LEND * FRAMES as u64,
        "{filed} allocations in {FRAMES} frames through a plugin with an attributes file"
    );
}

/// The allocations of `FRAMES` frames, each carrying `attributes`, through
/// a fresh chain on `ports` whose forwarding plugin loads `file`.
fn steady_allocs(ports: [&str; 2], attributes: &[&str], file: Option<&str>) -> u64 {
    let wiring = Arc::new(WiringRegistry::new());
    let seen = Arc::new(AtomicUsize::new(0));
    let tagged = Arc::new(AtomicUsize::new(0));
    let claims = ports.map(|port| asyn_rs::asyn_record::claim_port(port).unwrap());
    let (forward, _jf) = create_plugin_runtime(
        &claims[0],
        Forward,
        0,
        4,
        0,
        "",
        wiring.clone(),
        DEFAULT_PLUGIN_THREADS,
    );
    let (sink, _js) = create_plugin_runtime(
        &claims[1],
        Sink(seen.clone(), tagged.clone()),
        0,
        4,
        0,
        "",
        wiring,
        DEFAULT_PLUGIN_THREADS,
    );
    wire_downstream(&forward, sink.array_sender().clone());
    // An IOC past iocInit, whose callParamCallbacks post.
    let gate = InterruptGate::opened();
    for (claim, plugin) in claims.into_iter().zip([&forward, &sink]) {
        claim
            .publish(plugin.port_runtime().port_handle().clone(), &gate)
            .unwrap();
    }
    // The boot flushes are the gate's jobs, which iocInit's dbCaRun step
    // waits for too; left running, their threads start inside the count.
    test_block_on(gate.jobs_settled());
    if let Some(xml) = file {
        forward
            .port_runtime()
            .port_handle()
            .submit_blocking(
                asyn_rs::request::RequestOp::OctetWrite {
                    data: xml.as_bytes().to_vec(),
                },
                asyn_rs::user::AsynUser::new(forward.ndarray_params.attributes_file),
            )
            .unwrap();
    }
    enable(&forward);
    enable(&sink);
    let applied = [applied(&forward), applied(&sink)];

    let pool = DriverPool::standalone(0);
    let mut output = NDArrayOutput::new();
    output.add(forward.array_sender().clone());
    let publisher = ArrayPublisher::new(Arc::new(parking_lot::Mutex::new(output)));

    let mut window = None;
    for i in 0..WARM + FRAMES {
        if i == WARM {
            quiesce(&[&forward, &sink]);
            window = Some(named::open(&THREADS));
        }
        // The driver's `pArrays[0]`, released and allocated again
        // (simDetector.cpp:625-638); its fill is not the plugin path.
        let frame = exempt(|| {
            pool.owner()
                .replace_array(0, || {
                    let mut frame = pool
                        .alloc(
                            vec![NDDimension::new(64), NDDimension::new(64)],
                            NDDataType::UInt16,
                        )
                        .ok()?;
                    let mut fill = frame.fill();
                    fill.set_unique_id(i as i32 + 1);
                    for (k, name) in attributes.iter().enumerate() {
                        fill.attributes_mut().add(NDAttribute::new_static(
                            *name,
                            "driver attribute",
                            NDAttrSource::Driver,
                            NDAttrValue::Int32((i + k) as i32),
                        ));
                    }
                    Some(frame)
                })
                .unwrap()
        });
        publish(&publisher, frame);
        while seen.load(Ordering::Acquire) <= i
            || applied
                .iter()
                .any(|c| c.load(Ordering::Acquire) <= i as i32)
        {
            std::thread::yield_now();
        }
    }
    let [_, others] = window.unwrap().close();
    let evaluated = if file.is_some() { WARM + FRAMES } else { 0 };
    assert_eq!(
        tagged.load(Ordering::Relaxed),
        evaluated,
        "frames carrying Cnt"
    );
    others.allocs
}
