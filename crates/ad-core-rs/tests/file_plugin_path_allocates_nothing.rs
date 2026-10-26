//! What NDPluginFile's base does per frame allocates nothing once it is
//! warm, as C's `processCallbacks` and `writeFileBase` do: they set the
//! parameter library in place and build the file name in stack buffers
//! (NDPluginFile.cpp:201-364, :715-789).
//!
//! Each test counts every thread, so they take [`SERIAL`] in turn; frames
//! are published as `plugin_path_allocates_nothing` publishes them.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::task::{Context, Waker};
use std::time::Duration;

use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute, NDAttributeList};
use ad_core_rs::error::{ADError, ADResult};
use ad_core_rs::ndarray::{NDArray, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::{DriverPool, NDArrayPool, PublishedArray};
use ad_core_rs::plugin::channel::{ArrayPublisher, NDArrayOutput};
use ad_core_rs::plugin::file_base::{NDFileOpenMode, NDFileWriter};
use ad_core_rs::plugin::file_controller::{FileControl, FilePluginController};
use ad_core_rs::plugin::runtime::{
    ASYN_CANBLOCK, DEFAULT_PLUGIN_THREADS, NDPluginProcess, ParamPost, ParamUpdate,
    PluginAttributes, PluginRuntimeHandle, PortPost, ProcessResult, WriteOutcome, WritePort,
    WriteVerdict, create_plugin_runtime,
};
use ad_core_rs::plugin::wiring::WiringRegistry;
use asyn_rs::interrupt::InterruptFilter;
use asyn_rs::param::ParamValue;
use asyn_rs::port::{InterfaceMask, PortDriverBase};
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

/// Sets an empty WriteMessage and NumCaptured per frame, as a stream write
/// does (NDPluginFile.cpp:232, :778).
struct Setting {
    write_message: Arc<AtomicUsize>,
    num_captured: Arc<AtomicUsize>,
    frames: AtomicI32,
}

impl NDPluginProcess for Setting {
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ASYN_CANBLOCK;
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::GENERIC_POINTER;
    const INTERRUPT_MASK: InterfaceMask = Self::INTERFACE_MASK;

    fn process_array(&self, _array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        let frame = self.frames.fetch_add(1, Ordering::Relaxed) + 1;
        let mut result = ProcessResult::empty();
        let write_message = self.write_message.load(Ordering::Relaxed);
        let num_captured = self.num_captured.load(Ordering::Relaxed);
        result.param_updates.extend([
            ParamUpdate::octet(write_message, String::new()),
            ParamUpdate::int32(num_captured, frame),
        ]);
        result
    }

    fn plugin_type(&self) -> &str {
        "Setting"
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

/// Evaluates the plugin's `NDAttributesFile` for a writer once it has
/// recorded a set, as NDPluginFile's base does before each write that
/// reads it (NDFileHDF5.cpp:1421), then hands the frame on.
struct Viewing {
    num_captured: Arc<AtomicUsize>,
    frames: AtomicI32,
    written: Arc<Mutex<NDAttributeList>>,
    attributes: Option<PluginAttributes>,
}

impl NDPluginProcess for Viewing {
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ASYN_CANBLOCK;
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::GENERIC_POINTER;
    const INTERRUPT_MASK: InterfaceMask = Self::INTERFACE_MASK;

    fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        let mut post = PortPost::of(self.attributes.as_ref());
        let view = post
            .attribute_view()
            .expect("the attributes file is loaded");
        let frame = self.frames.fetch_add(1, Ordering::Relaxed) + 1;
        let num_captured = self.num_captured.load(Ordering::Relaxed);
        view.record([(num_captured, 0, ParamValue::Int32(frame))]);
        view.with_evaluated(|list| self.written.lock().clone_from(list));
        post.hand_on(array);
        ProcessResult::sink(Vec::new())
    }

    fn bind_attributes(&mut self, attributes: PluginAttributes) {
        self.attributes = Some(attributes);
    }

    fn array_callbacks_seed(&self) -> Option<bool> {
        Some(false)
    }

    fn plugin_type(&self) -> &str {
        "Viewing"
    }
}

/// A multi-array format that writes nothing: it counts its frames and those
/// whose plugin set carried `Cnt`.
struct Discard {
    written: Arc<AtomicUsize>,
    tagged: Arc<AtomicUsize>,
    attributes: NDAttributeList,
}

impl NDFileWriter for Discard {
    fn open_file(&mut self, _path: &Path, _mode: NDFileOpenMode, _array: &NDArray) -> ADResult<()> {
        Ok(())
    }
    fn write_file(&mut self, _array: &Arc<NDArray>) -> ADResult<()> {
        if self.attributes.get("Cnt").is_some() {
            self.tagged.fetch_add(1, Ordering::Relaxed);
        }
        self.written.fetch_add(1, Ordering::Release);
        Ok(())
    }
    fn read_file(&mut self, _pool: &NDArrayPool) -> ADResult<NDArray> {
        Err(ADError::UnsupportedConversion("write only".into()))
    }
    fn close_file(&mut self) -> ADResult<()> {
        Ok(())
    }
    fn set_plugin_attributes(&mut self, attributes: &NDAttributeList) {
        self.attributes.clone_from(attributes);
    }
}

/// NDPluginFile over [`Discard`], wired as the file plugins wire theirs.
struct FilePlugin {
    ctrl: Mutex<FilePluginController<Discard>>,
    attributes: Option<PluginAttributes>,
}

impl NDPluginProcess for FilePlugin {
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ASYN_CANBLOCK;
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::GENERIC_POINTER;
    const INTERRUPT_MASK: InterfaceMask = Self::INTERFACE_MASK;

    fn validate_int32(
        &self,
        reason: usize,
        _addr: i32,
        value: i32,
        verdict: WriteVerdict<i32>,
        params: &mut WritePort<'_>,
    ) -> WriteOutcome<i32> {
        self.ctrl.lock().write_int32(reason, value, verdict, params)
    }

    fn validate_octet(
        &self,
        reason: usize,
        _addr: i32,
        _value: &str,
        verdict: WriteVerdict<String>,
        _params: &PortDriverBase,
    ) -> WriteOutcome<String> {
        self.ctrl.lock().write_octet(reason, verdict)
    }

    fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        let mut post = PortPost::of(self.attributes.as_ref());
        self.ctrl.lock().process_array(array, &mut post)
    }

    fn bind_attributes(&mut self, attributes: PluginAttributes) {
        self.attributes = Some(attributes);
    }

    fn array_callbacks_seed(&self) -> Option<bool> {
        Some(false)
    }

    fn plugin_type(&self) -> &str {
        "Discard"
    }

    fn register_params(&mut self, base: &mut PortDriverBase) -> asyn_rs::error::AsynResult<()> {
        self.ctrl.lock().register_params(base)
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

fn write_octet(plugin: &PluginRuntimeHandle, reason: usize, v: &str) {
    plugin
        .port_runtime()
        .port_handle()
        .submit_blocking(
            asyn_rs::request::RequestOp::OctetWrite {
                data: v.as_bytes().to_vec(),
            },
            asyn_rs::user::AsynUser::new(reason),
        )
        .unwrap();
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

/// A plugin's own `NDAttributesFile`.
const PLUGIN_ATTRIBUTES: &str = r#"<Attributes>
    <Attribute name="Cnt" type="PARAM" source="ARRAY_COUNTER" datatype="INT"/>
    <Attribute name="Num" type="PARAM" source="NUM_CAPTURED" datatype="INT"/>
    <Attribute name="Tag" type="CONST" source="abc" datatype="STRING"/>
</Attributes>"#;

/// What a driver's `getAttributes` puts on each frame (simDetector.cpp:828).
const DRIVER_ATTRIBUTES: [&str; 4] = ["ColorMode", "BayerPattern", "Gain", "Exposure"];

/// A lend of a port to a caller's thread: its cancel token, its reply
/// channel and the no-op request the actor parks on.
const LEND: usize = 3;

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
fn setting_a_frames_params_allocates_nothing() {
    let _serial = SERIAL.lock();
    let write_message = Arc::new(AtomicUsize::new(usize::MAX));
    let num_captured = Arc::new(AtomicUsize::new(usize::MAX));
    let plugin = plugin(
        "ZALLOC_SETS",
        Setting {
            write_message: write_message.clone(),
            num_captured: num_captured.clone(),
            frames: AtomicI32::new(0),
        },
    );
    let p = &plugin.ndarray_params;
    write_message.store(p.file_write_message, Ordering::Relaxed);
    num_captured.store(p.num_captured, Ordering::Relaxed);
    let allocs = steady_allocs(&plugin, &[], |_| true);
    let set = plugin
        .port_runtime()
        .port_handle()
        .read_int32_blocking(plugin.ndarray_params.num_captured, 0)
        .unwrap();
    assert_eq!(set, (WARM + FRAMES) as i32, "the last frame's set");
    assert_eq!(allocs, 0, "{FRAMES} frames after {WARM} warm-up ones");
}

#[test]
fn handing_a_frame_on_allocates_nothing() {
    let _serial = SERIAL.lock();
    let plugin = plugin("ZALLOC_HANDON", Handing { attributes: None });
    let allocs = steady_allocs(&plugin, &[], |_| true);
    assert_eq!(allocs, 0, "{FRAMES} frames after {WARM} warm-up ones");
}

#[test]
fn evaluating_a_plugins_attributes_for_its_writer_allocates_only_the_lend() {
    let _serial = SERIAL.lock();
    let num_captured = Arc::new(AtomicUsize::new(usize::MAX));
    let written = Arc::new(Mutex::new(NDAttributeList::new()));
    let plugin = plugin(
        "ZALLOC_VIEW",
        Viewing {
            num_captured: num_captured.clone(),
            frames: AtomicI32::new(0),
            written: written.clone(),
            attributes: None,
        },
    );
    num_captured.store(plugin.ndarray_params.num_captured, Ordering::Relaxed);
    write_octet(
        &plugin,
        plugin.ndarray_params.attributes_file,
        PLUGIN_ATTRIBUTES,
    );
    let allocs = steady_allocs(&plugin, &[], |_| true);
    let written = written.lock();
    let value = |name| written.get(name).map(|a| a.value.clone());
    let last = NDAttrValue::Int32((WARM + FRAMES) as i32);
    assert_eq!(value("Cnt"), Some(last.clone()), "the frame's ArrayCounter");
    assert_eq!(value("Num"), Some(last), "the set the frame recorded");
    assert_eq!(value("Tag"), Some(NDAttrValue::String("abc".into())));
    assert!(
        allocs <= LEND * FRAMES,
        "{allocs} allocations in {FRAMES} frames after {WARM} warm-up ones"
    );
}

#[test]
fn a_streamed_frame_allocates_nothing_in_the_file_plugin() {
    let _serial = SERIAL.lock();
    let bare = streamed("ZALLOC_STREAM", &[], None);
    assert_eq!(bare, 0, "{FRAMES} bare frames after {WARM} warm-up ones");
    let attributed = streamed("ZALLOC_ASTREAM", &DRIVER_ATTRIBUTES, None);
    assert_eq!(
        attributed, 0,
        "{FRAMES} frames with driver attributes after {WARM} warm-up ones"
    );
    let filed = streamed("ZALLOC_FSTREAM", &[], Some(PLUGIN_ATTRIBUTES));
    assert!(
        filed <= LEND * FRAMES,
        "{filed} allocations in {FRAMES} frames through a file plugin with an attributes file"
    );
}

/// The allocations of `FRAMES` frames, each carrying `attributes`, streamed
/// into an open file by a fresh file plugin on `port` that loads `file`.
fn streamed(port: &str, attributes: &[&str], file: Option<&str>) -> usize {
    let written = Arc::new(AtomicUsize::new(0));
    let tagged = Arc::new(AtomicUsize::new(0));
    let plugin = plugin(
        port,
        FilePlugin {
            ctrl: Mutex::new(FilePluginController::new(Discard {
                written: written.clone(),
                tagged: tagged.clone(),
                attributes: NDAttributeList::new(),
            })),
            attributes: None,
        },
    );
    if let Some(xml) = file {
        write_octet(&plugin, plugin.ndarray_params.attributes_file, xml);
    }
    let p = &plugin.ndarray_params;
    write_octet(&plugin, p.file_path, env!("CARGO_TARGET_TMPDIR"));
    write_octet(&plugin, p.file_name, "zalloc");
    write_octet(&plugin, p.file_template, "%s%s_%3.3d.dat");
    write_i32(&plugin, p.file_write_mode, 2);
    write_i32(&plugin, p.num_capture, 0);
    write_i32(&plugin, p.lazy_open, 1);
    write_i32(&plugin, p.capture, 1);
    let allocs = steady_allocs(&plugin, attributes, |i| written.load(Ordering::Acquire) > i);
    let evaluated = if file.is_some() { WARM + FRAMES } else { 0 };
    assert_eq!(
        tagged.load(Ordering::Relaxed),
        evaluated,
        "frames written with Cnt"
    );
    allocs
}
