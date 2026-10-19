use std::sync::mpsc;
use std::time::Duration;

use parking_lot::Mutex;

/// Whether `write` returns while `writer` is held, as by a file write in
/// progress.
pub(crate) fn returns_while_busy<W>(writer: &Mutex<W>, write: impl FnOnce() + Send) -> bool {
    let busy = writer.lock();
    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        scope.spawn(move || {
            write();
            tx.send(()).unwrap();
        });
        let returned = rx.recv_timeout(Duration::from_secs(5)).is_ok();
        drop(busy);
        returned
    })
}

/// A running `processor` with `xml` loaded as its `NDAttributesFile`: the
/// handle, what its address-0 output delivers, and its data thread.
pub(crate) fn with_attributes_file<P: ad_core_rs::plugin::runtime::NDPluginProcess>(
    port: &str,
    processor: P,
    xml: &str,
) -> (
    ad_core_rs::plugin::runtime::PluginRuntimeHandle,
    ad_core_rs::plugin::channel::NDArrayReceiver,
    impl Sized + use<P>,
) {
    let (handle, rx, jh) = running(port, processor);
    let port = handle.port_runtime().port_handle();
    port.submit_blocking(
        asyn_rs::request::RequestOp::OctetWrite {
            data: xml.as_bytes().to_vec(),
        },
        asyn_rs::user::AsynUser::new(handle.ndarray_params.attributes_file),
    )
    .unwrap();
    assert!(handle.wait_params_applied(Duration::from_secs(10)));
    (handle, rx, jh)
}

/// Publish `array` to `handle` as a driver would.
pub(crate) fn publish(
    handle: &ad_core_rs::plugin::runtime::PluginRuntimeHandle,
    array: &ad_core_rs::ndarray::NDArray,
) {
    let array = ad_core_rs::ndarray_pool::DriverPool::standalone(0)
        .alloc_copy(array)
        .unwrap()
        .share();
    let sender = handle.array_sender().clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(sender.publish(array));
    })
    .join()
    .unwrap();
}

/// Point a file plugin's `FilePath`/`FileName`/`FileTemplate` at `file` and
/// set `AutoSave`, so every published frame is written there.
pub(crate) fn auto_save_to(
    handle: &ad_core_rs::plugin::runtime::PluginRuntimeHandle,
    file: &std::path::Path,
) {
    let port = handle.port_runtime().port_handle();
    let params = &handle.ndarray_params;
    let dir = format!("{}/", file.parent().unwrap().display());
    let name = file.file_name().unwrap().to_str().unwrap();
    for (reason, value) in [
        (params.file_path, dir.as_str()),
        (params.file_name, name),
        (params.file_template, "%s%s"),
    ] {
        port.submit_blocking(
            asyn_rs::request::RequestOp::OctetWrite {
                data: value.as_bytes().to_vec(),
            },
            asyn_rs::user::AsynUser::new(reason),
        )
        .unwrap();
    }
    port.write_int32_blocking(params.auto_save, 0, 1).unwrap();
    assert!(handle.wait_params_applied(Duration::from_secs(10)));
}

/// Wait until a file plugin has taken `n` frames and finished writing them.
/// ArrayCounter is posted with WriteFile 1, before the write, as C's is
/// (NDPluginFile.cpp:239-240), so the drained queue is what says the write
/// ended.
pub(crate) fn wait_saved(handle: &ad_core_rs::plugin::runtime::PluginRuntimeHandle, n: i32) {
    let port = handle.port_runtime().port_handle();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while port
        .read_int32_blocking(handle.ndarray_params.array_counter, 0)
        .unwrap()
        < n
    {
        assert!(std::time::Instant::now() < deadline, "frame {n} not saved");
        std::thread::yield_now();
    }
    assert!(handle.wait_params_applied(Duration::from_secs(10)));
}

/// A running `processor` with callbacks on: the handle, what its address-0
/// output delivers, and its data thread.
pub(crate) fn running<P: ad_core_rs::plugin::runtime::NDPluginProcess>(
    port: &str,
    processor: P,
) -> (
    ad_core_rs::plugin::runtime::PluginRuntimeHandle,
    ad_core_rs::plugin::channel::NDArrayReceiver,
    impl Sized + use<P>,
) {
    start(&asyn_rs::asyn_record::claim_port(port).unwrap(), processor)
}

/// [`running`], registered with an open `interruptAccept` as after iocInit,
/// so its posts reach interrupt subscribers.
pub(crate) fn registered<P: ad_core_rs::plugin::runtime::NDPluginProcess>(
    port: &str,
    processor: P,
) -> (
    ad_core_rs::plugin::runtime::PluginRuntimeHandle,
    ad_core_rs::plugin::channel::NDArrayReceiver,
    impl Sized + use<P>,
) {
    let claim = asyn_rs::asyn_record::claim_port(port).unwrap();
    let started = start(&claim, processor);
    let handle = started.0.port_runtime().port_handle().clone();
    claim
        .publish(handle, &asyn_rs::registry::open_owner())
        .unwrap();
    started
}

fn start<P: ad_core_rs::plugin::runtime::NDPluginProcess>(
    claim: &asyn_rs::registry::PortClaim,
    processor: P,
) -> (
    ad_core_rs::plugin::runtime::PluginRuntimeHandle,
    ad_core_rs::plugin::channel::NDArrayReceiver,
    impl Sized + use<P>,
) {
    use ad_core_rs::plugin::channel::{NDArrayOutput, ndarray_channel};
    let (sender, rx) = ndarray_channel("DOWNSTREAM", 10);
    let mut output = NDArrayOutput::new();
    output.add(sender);
    let (handle, jh) = ad_core_rs::plugin::runtime::create_plugin_runtime_with_output(
        claim,
        processor,
        10_000_000,
        10,
        0,
        output,
        "",
        std::sync::Arc::new(ad_core_rs::plugin::wiring::WiringRegistry::new()),
        ad_core_rs::plugin::runtime::DEFAULT_PLUGIN_THREADS,
    );
    let port = handle.port_runtime().port_handle();
    port.write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
        .unwrap();
    assert!(handle.wait_params_applied(Duration::from_secs(10)));
    (handle, rx, jh)
}

/// The `beginProcessCallbacks` params a plugin posted for one frame.
#[derive(Debug)]
pub(crate) struct BeginParams {
    pub n_dimensions: i32,
    pub data_type: i32,
    pub color_mode: i32,
    pub unique_id: i32,
    pub compressed_size: i32,
    pub codec: Vec<u8>,
}

/// Run `processor` with callbacks on, publish `input` to it, and read back
/// what its frame posted.
pub(crate) fn begin_params<P: ad_core_rs::plugin::runtime::NDPluginProcess>(
    port: &str,
    processor: P,
    input: &ad_core_rs::ndarray::NDArray,
) -> BeginParams {
    let (handle, _rx, _jh) = running(port, processor);
    let port = handle.port_runtime().port_handle();
    publish(&handle, input);
    // One batch carries ArrayCounter and the rest.
    let p = &handle.ndarray_params;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while port.read_int32_blocking(p.array_counter, 0).ok() != Some(1) {
        assert!(std::time::Instant::now() < deadline, "frame not posted");
        std::thread::yield_now();
    }
    let int = |reason| port.read_int32_blocking(reason, 0).unwrap();
    let codec = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(port.read_octet(p.codec, 0, 64))
        .unwrap();
    BeginParams {
        n_dimensions: int(p.n_dimensions),
        data_type: int(p.data_type),
        color_mode: int(p.color_mode),
        unique_id: int(p.unique_id),
        compressed_size: int(p.compressed_size),
        codec,
    }
}

/// Run `processor` with callbacks on, publish each of `inputs`, and read
/// `[ArraySizeX, ArraySizeY, ArraySizeZ, ArraySize]` after each frame.
pub(crate) fn array_sizes<P: ad_core_rs::plugin::runtime::NDPluginProcess>(
    port: &str,
    processor: P,
    inputs: &[ad_core_rs::ndarray::NDArray],
) -> Vec<[i32; 4]> {
    let (handle, _rx, _jh) = running(port, processor);
    let port = handle.port_runtime().port_handle();
    let p = &handle.ndarray_params;
    let mut sizes = Vec::new();
    for (n, input) in (1..).zip(inputs) {
        publish(&handle, input);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while port.read_int32_blocking(p.array_counter, 0).ok() != Some(n) {
            assert!(std::time::Instant::now() < deadline, "frame {n} not posted");
            std::thread::yield_now();
        }
        sizes.push(
            [p.array_size_x, p.array_size_y, p.array_size_z, p.array_size]
                .map(|reason| port.read_int32_blocking(reason, 0).unwrap()),
        );
    }
    sizes
}

/// A downstream plugin that reports each frame, then waits for `gate`.
pub(crate) struct Gated {
    gate: std::sync::Arc<Mutex<()>>,
    entered: mpsc::Sender<i32>,
}

impl ad_core_rs::plugin::runtime::NDPluginProcess for Gated {
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ad_core_rs::plugin::runtime::ASYN_CANBLOCK;
    const INTERFACE_MASK: asyn_rs::port::InterfaceMask =
        asyn_rs::port::InterfaceMask::UNMIGRATED_SUBCLASS;
    const INTERRUPT_MASK: asyn_rs::port::InterfaceMask =
        asyn_rs::port::InterfaceMask::UNMIGRATED_SUBCLASS;
    fn process_array(
        &self,
        array: &std::sync::Arc<ad_core_rs::ndarray::NDArray>,
        _pool: &ad_core_rs::ndarray_pool::NDArrayPool,
    ) -> ad_core_rs::plugin::runtime::ProcessResult {
        let _ = self.entered.send(array.unique_id);
        drop(self.gate.lock());
        ad_core_rs::plugin::runtime::ProcessResult::sink(vec![])
    }
    fn plugin_type(&self) -> &str {
        "Gated"
    }
}

/// A file plugin saving to `path`, handing on to a BlockingCallbacks
/// downstream that waits on the returned gate, reporting each frame it is
/// handed on the returned receiver. The plugin has an attributes file, so
/// handing a frame on waits on its port's actor.
pub(crate) fn gated<P: ad_core_rs::plugin::runtime::NDPluginProcess>(
    port: &str,
    processor: P,
    path: &std::path::Path,
) -> (
    Vec<Box<dyn std::any::Any>>,
    ad_core_rs::plugin::runtime::PluginRuntimeHandle,
    std::sync::Arc<Mutex<()>>,
    mpsc::Receiver<i32>,
) {
    let gate = std::sync::Arc::new(Mutex::new(()));
    let (entered, rx) = mpsc::channel();
    let (down, down_rx, down_jh) = running(
        &format!("{port}_DS"),
        Gated {
            gate: gate.clone(),
            entered,
        },
    );
    down.port_runtime()
        .port_handle()
        .write_int32_blocking(down.plugin_params.blocking_callbacks, 0, 1)
        .unwrap();
    assert!(down.wait_params_applied(Duration::from_secs(10)));
    let xml = r#"<Attributes>
        <Attribute name="Counter" type="PARAM" source="ARRAY_COUNTER" datatype="INT"/>
    </Attributes>"#;
    let (handle, up_rx, up_jh) = with_attributes_file(port, processor, xml);
    ad_core_rs::plugin::runtime::wire_downstream(&handle, down.array_sender().clone());
    handle
        .port_runtime()
        .port_handle()
        .write_int32_blocking(handle.ndarray_params.array_callbacks, 0, 1)
        .unwrap();
    auto_save_to(&handle, path);
    let keep: Vec<Box<dyn std::any::Any>> = vec![
        Box::new(down),
        Box::new(down_rx),
        Box::new(down_jh),
        Box::new(up_rx),
        Box::new(up_jh),
    ];
    (keep, handle, gate, rx)
}

/// Write WriteFile 1 on its own thread and run `while_running`; true if the
/// write returned in 10 s.
pub(crate) fn write_file_returns(
    handle: &ad_core_rs::plugin::runtime::PluginRuntimeHandle,
    while_running: impl FnOnce(),
) -> bool {
    let port = handle.port_runtime().port_handle().clone();
    let reason = handle.ndarray_params.write_file;
    let (done, returned) = mpsc::channel();
    std::thread::spawn(move || {
        port.write_int32_blocking(reason, 0, 1).unwrap();
        let _ = done.send(());
    });
    while_running();
    returned.recv_timeout(Duration::from_secs(10)).is_ok()
}

/// A four-byte UInt8 frame with `id` as its uniqueId.
pub(crate) fn frame(id: i32) -> ad_core_rs::ndarray::NDArray {
    let mut arr = ad_core_rs::ndarray::NDArray::new(
        vec![ad_core_rs::ndarray::NDDimension::new(4)],
        ad_core_rs::ndarray::NDDataType::UInt8,
    );
    arr.unique_id = id;
    arr
}
