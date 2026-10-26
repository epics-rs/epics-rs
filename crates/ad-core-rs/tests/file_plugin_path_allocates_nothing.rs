//! What NDPluginFile's base does per frame allocates nothing once it is
//! warm, as C's `processCallbacks` and `writeFileBase` do: they set the
//! parameter library in place and build the file name in stack buffers
//! (NDPluginFile.cpp:201-364, :715-789).
//!
//! Each test counts every thread, so they take [`SERIAL`] in turn; frames
//! are published as `plugin_path_allocates_nothing` publishes them.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::task::{Context, Waker};
use std::time::Duration;

use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
use ad_core_rs::ndarray::{NDArray, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::{DriverPool, NDArrayPool, PublishedArray};
use ad_core_rs::plugin::channel::{ArrayPublisher, NDArrayOutput};
use ad_core_rs::plugin::runtime::{
    ASYN_CANBLOCK, DEFAULT_PLUGIN_THREADS, NDPluginProcess, ParamPost, PluginAttributes,
    PluginRuntimeHandle, PortPost, ProcessResult, create_plugin_runtime,
};
use ad_core_rs::plugin::wiring::WiringRegistry;
use asyn_rs::interrupt::InterruptFilter;
use asyn_rs::param::ParamValue;
use asyn_rs::port::InterfaceMask;
use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;
use parking_lot::Mutex;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);
static SERIAL: Mutex<()> = Mutex::new(());

thread_local! {
    /// The driver's own work on the publishing thread, which is not the
    /// plugin path.
    static EXEMPT: Cell<bool> = const { Cell::new(false) };
}

struct Counting;

impl Counting {
    fn note(&self) {
        if COUNTING.load(Ordering::Relaxed) && !EXEMPT.try_with(Cell::get).unwrap_or(false) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// SAFETY: forwards every call to `System` unchanged; the counter and the
// const-initialised thread-local never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.note();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.note();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        self.note();
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn exempt<R>(f: impl FnOnce() -> R) -> R {
    EXEMPT.with(|e| e.set(true));
    let r = f();
    EXEMPT.with(|e| e.set(false));
    r
}

/// Reads its frame's ArrayCounter through a [`PortPost`], as a capture
/// flush starts by reading it (NDPluginFile.cpp:283-287).
struct CounterReader {
    reason: Arc<AtomicUsize>,
    read: Arc<AtomicI32>,
    attributes: Option<PluginAttributes>,
}

impl NDPluginProcess for CounterReader {
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ASYN_CANBLOCK;
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::GENERIC_POINTER;
    const INTERRUPT_MASK: InterfaceMask = Self::INTERFACE_MASK;

    fn process_array(&self, _array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        let post = PortPost::of(self.attributes.as_ref());
        if let Some(count) = post.int32(self.reason.load(Ordering::Relaxed)) {
            self.read.store(count, Ordering::Release);
        }
        ProcessResult::sink(Vec::new())
    }

    fn bind_attributes(&mut self, attributes: PluginAttributes) {
        self.attributes = Some(attributes);
    }

    fn plugin_type(&self) -> &str {
        "CounterReader"
    }
}

/// Hands each frame on, as a stream write hands on what it wrote
/// (NDPluginFile.cpp:252), with NDArrayCallbacks off as a file plugin
/// starts (NDPluginFile.cpp:948).
struct Handing {
    attributes: Option<PluginAttributes>,
}

impl NDPluginProcess for Handing {
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ASYN_CANBLOCK;
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::GENERIC_POINTER;
    const INTERRUPT_MASK: InterfaceMask = Self::INTERFACE_MASK;

    fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        PortPost::of(self.attributes.as_ref()).hand_on(array);
        ProcessResult::sink(Vec::new())
    }

    fn bind_attributes(&mut self, attributes: PluginAttributes) {
        self.attributes = Some(attributes);
    }

    fn array_callbacks_seed(&self) -> Option<bool> {
        Some(false)
    }

    fn plugin_type(&self) -> &str {
        "Handing"
    }
}

fn write_i32(plugin: &PluginRuntimeHandle, reason: usize, v: i32) {
    plugin
        .port_runtime()
        .port_handle()
        .write_int32_blocking(reason, 0, v)
        .unwrap();
    assert!(plugin.wait_params_applied(Duration::from_secs(10)));
}

/// `processor` on a fresh plugin port, its callbacks enabled.
fn plugin<P: NDPluginProcess>(port: &str, processor: P) -> PluginRuntimeHandle {
    let claim = asyn_rs::asyn_record::claim_port(port).unwrap();
    let (plugin, _jh) = create_plugin_runtime(
        &claim,
        processor,
        0,
        4,
        0,
        "",
        Arc::new(WiringRegistry::new()),
        DEFAULT_PLUGIN_THREADS,
    );
    claim
        .publish(
            plugin.port_runtime().port_handle().clone(),
            &InterruptGate::opened(),
        )
        .unwrap();
    write_i32(&plugin, plugin.plugin_params.enable_callbacks, 1);
    plugin
}

/// The `ArrayCounter` the plugin's port has last applied.
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

/// The allocations of `FRAMES` frames, each carrying `attributes`, through
/// `plugin` after `WARM` warm-up ones. Frame `i` is done once its port has
/// applied its ArrayCounter and `done(i)`.
fn steady_allocs(
    plugin: &PluginRuntimeHandle,
    attributes: &[&str],
    done: impl Fn(usize) -> bool,
) -> usize {
    let applied = applied(plugin);
    let pool = DriverPool::standalone(0);
    let mut output = NDArrayOutput::new();
    output.add(plugin.array_sender().clone());
    let publisher = ArrayPublisher::new(Arc::new(parking_lot::Mutex::new(output)));

    for i in 0..WARM + FRAMES {
        if i == WARM {
            exempt(|| {
                assert!(plugin.wait_params_applied(Duration::from_secs(10)));
                plugin
                    .port_runtime()
                    .port_handle()
                    .read_int32_blocking(plugin.ndarray_params.array_counter, 0)
                    .unwrap();
            });
            ALLOCS.store(0, Ordering::SeqCst);
            COUNTING.store(true, Ordering::SeqCst);
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
        while !done(i) || applied.load(Ordering::Acquire) <= i as i32 {
            std::thread::yield_now();
        }
    }
    COUNTING.store(false, Ordering::SeqCst);
    ALLOCS.load(Ordering::SeqCst)
}

#[test]
fn reading_a_frames_begin_sets_allocates_nothing() {
    let _serial = SERIAL.lock();
    let reason = Arc::new(AtomicUsize::new(usize::MAX));
    let read = Arc::new(AtomicI32::new(0));
    let plugin = plugin(
        "ZALLOC_BEGIN",
        CounterReader {
            reason: reason.clone(),
            read: read.clone(),
            attributes: None,
        },
    );
    reason.store(plugin.ndarray_params.array_counter, Ordering::Relaxed);
    let allocs = steady_allocs(&plugin, &[], |_| true);
    assert_eq!(
        read.load(Ordering::Acquire),
        (WARM + FRAMES) as i32,
        "the last frame's ArrayCounter"
    );
    assert_eq!(allocs, 0, "{FRAMES} frames after {WARM} warm-up ones");
}

#[test]
fn handing_a_frame_on_allocates_nothing() {
    let _serial = SERIAL.lock();
    let plugin = plugin("ZALLOC_HANDON", Handing { attributes: None });
    let allocs = steady_allocs(&plugin, &[], |_| true);
    assert_eq!(allocs, 0, "{FRAMES} frames after {WARM} warm-up ones");
}
