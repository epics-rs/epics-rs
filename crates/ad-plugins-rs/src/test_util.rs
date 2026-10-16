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
    impl Sized,
) {
    use ad_core_rs::plugin::channel::{NDArrayOutput, ndarray_channel};
    let (sender, rx) = ndarray_channel("DOWNSTREAM", 10);
    let mut output = NDArrayOutput::new();
    output.add(sender);
    let (handle, jh) = ad_core_rs::plugin::runtime::create_plugin_runtime_with_output(
        &asyn_rs::asyn_record::claim_port(port).unwrap(),
        processor,
        10_000_000,
        10,
        output,
        "",
        std::sync::Arc::new(ad_core_rs::plugin::wiring::WiringRegistry::new()),
        ad_core_rs::plugin::runtime::DEFAULT_PLUGIN_THREADS,
    );
    let port = handle.port_runtime().port_handle();
    port.write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
        .unwrap();
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

/// Wait until a file plugin's ArrayCounter, which counts saved frames, has
/// reached `n`.
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
}
