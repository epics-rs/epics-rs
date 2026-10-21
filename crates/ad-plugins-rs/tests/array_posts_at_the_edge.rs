//! C posts TSTimeAxis from the first frame's `allocateArrays`
//! (NDPluginTimeSeries.cpp:410-422, createAxisArray :160) and FFTTimeAxis /
//! FFTFreqAxis from the first frame's size change (NDPluginFFT.cpp:118,
//! :253-254). A record drops such a post while `interruptAccept` is false
//! (devAsynXXXArray.cpp:407-410) and the sweep at the edge posts no array
//! (asynPortDriver.cpp:846-865), so a first frame that beat the gate left the
//! axis at NORD 0 for good: rs t4u-em, whose reader drains the device faster
//! than C's 1 byte per ms (drvT4U_EM.cpp:795-796), lost it on every TS port.
//! NDDimensions, posted only when the dims change (NDPluginDriver.cpp:
//! 245-246), left every plugin's Dimensions_RBV (NDArrayBase.template:340)
//! empty the same way.

use std::sync::{Arc, Mutex};

use ad_core_rs::ndarray::{NDArray, NDDataBuffer, NDDimension};
use ad_core_rs::plugin::runtime::{DEFAULT_PLUGIN_THREADS, NDPluginProcess, create_plugin_runtime};
use ad_core_rs::plugin::wiring::WiringRegistry;
use ad_plugins_rs::fft::FFTProcessor;
use ad_plugins_rs::time_series_plugin::TimeSeriesProcessor;
use asyn_rs::interrupt::InterruptFilter;
use asyn_rs::param::ParamValue;
use asyn_rs::port::DrvUserRequest;
use epics_base_rs::runtime::task::test_block_on;
use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;

enum Frame {
    None,
    BeforeTheGate(Vec<f64>),
    AfterTheGate(Vec<f64>),
}

/// The `(param, value)` posts a record on each of `params` receives.
fn array_posts_a_record_receives<P: NDPluginProcess>(
    port_name: &str,
    processor: P,
    params: &[&'static str],
    frame: Frame,
) -> Vec<(&'static str, Vec<f64>)> {
    let claim = asyn_rs::asyn_record::claim_port(port_name).unwrap();
    let (plugin, _jh) = create_plugin_runtime(
        &claim,
        processor,
        1_000_000,
        10,
        0,
        "",
        Arc::new(WiringRegistry::new()),
        DEFAULT_PLUGIN_THREADS,
    );
    let port = plugin.port_runtime().port_handle().clone();
    let gate = InterruptGate::new();
    claim.publish(port.clone(), &gate).unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    // A record's callback returns while the gate is closed.
    let _records: Vec<_> = params
        .iter()
        .map(|&name| {
            let reason = port
                .drv_user_create_blocking(&DrvUserRequest::new(name, 0))
                .unwrap()
                .reason;
            let (sink, record_gate) = (seen.clone(), gate.clone());
            port.interrupts()
                .register_sync_callback(
                    InterruptFilter {
                        reason: Some(reason),
                        addr: Some(0),
                        ..Default::default()
                    },
                    move |iv| {
                        let value = match &iv.value {
                            ParamValue::Float64Array(a) => a.to_vec(),
                            ParamValue::Int32Array(a) => a.iter().map(|&v| f64::from(v)).collect(),
                            _ => return,
                        };
                        if record_gate.is_open() {
                            sink.lock().unwrap().push((name, value));
                        }
                    },
                )
                .unwrap()
        })
        .collect();

    let applied = || assert!(plugin.wait_params_applied(std::time::Duration::from_secs(10)));
    port.write_int32_blocking(plugin.plugin_params.enable_callbacks, 0, 1)
        .unwrap();
    applied();
    let open = || {
        gate.set(true);
        test_block_on(gate.jobs_settled());
    };
    let (before, values) = match frame {
        Frame::None => (true, None),
        Frame::BeforeTheGate(values) => (true, Some(values)),
        Frame::AfterTheGate(values) => (false, Some(values)),
    };
    if !before {
        open();
    }
    if let Some(values) = values {
        let frame = NDArray::with_data(
            vec![NDDimension::new(values.len())],
            NDDataBuffer::F64(values),
        );
        let frame = ad_core_rs::ndarray_pool::DriverPool::standalone(0)
            .alloc_copy(&frame)
            .unwrap();
        test_block_on(plugin.array_sender().clone().publish(frame));
        applied();
        // The frame's posts are a port request the data thread queued before
        // it acked; a request queued behind them lands after them.
        port.call_param_callbacks_blocking(0).unwrap();
    }
    if before {
        open();
    }
    seen.lock().unwrap().clone()
}

fn lengths(posts: Vec<(&'static str, Vec<f64>)>) -> Vec<(&'static str, usize)> {
    posts.into_iter().map(|(name, v)| (name, v.len())).collect()
}

/// Boundary: a frame processed while the gate was closed.
#[test]
fn a_time_series_frame_before_the_gate_has_its_axis_posted_at_the_edge() {
    let posts = array_posts_a_record_receives(
        "TSAXEDGE1",
        TimeSeriesProcessor::new(1),
        &["TS_TIME_AXIS"],
        Frame::BeforeTheGate(vec![1.0]),
    );
    assert_eq!(lengths(posts), [("TS_TIME_AXIS", 2048)]);
}

/// Boundary: no frame yet. C's constructor axis (:160) reaches no record,
/// and nothing posts one at the edge.
#[test]
fn no_time_series_frame_before_the_gate_posts_no_axis_at_the_edge() {
    let posts = array_posts_a_record_receives(
        "TSAXEDGE0",
        TimeSeriesProcessor::new(1),
        &["TS_TIME_AXIS"],
        Frame::None,
    );
    assert!(posts.is_empty(), "{posts:?}");
}

/// Boundary: a frame processed while the gate was closed.
#[test]
fn an_fft_frame_before_the_gate_has_its_axes_posted_at_the_edge() {
    let posts = array_posts_a_record_receives(
        "FFTAXEDGE1",
        FFTProcessor::new(),
        &["FFT_TIME_AXIS", "FFT_FREQ_AXIS"],
        Frame::BeforeTheGate(vec![1.0; 8]),
    );
    assert_eq!(lengths(posts), [("FFT_TIME_AXIS", 8), ("FFT_FREQ_AXIS", 4)]);
}

/// Boundary: no frame yet; C makes FFT axes only from a frame.
#[test]
fn no_fft_frame_before_the_gate_posts_no_axes_at_the_edge() {
    let posts = array_posts_a_record_receives(
        "FFTAXEDGE0",
        FFTProcessor::new(),
        &["FFT_TIME_AXIS", "FFT_FREQ_AXIS"],
        Frame::None,
    );
    assert!(posts.is_empty(), "{posts:?}");
}

/// Boundary: a frame processed while the gate was closed. The post is C's
/// full `dimsPrev_`, ten slots (NDPluginDriver.cpp:246).
#[test]
fn a_frame_before_the_gate_has_its_dimensions_posted_at_the_edge() {
    let posts = array_posts_a_record_receives(
        "DIMSEDGE1",
        FFTProcessor::new(),
        &["ARRAY_DIMENSIONS"],
        Frame::BeforeTheGate(vec![1.0; 8]),
    );
    let mut dims = vec![0.0; 10];
    dims[0] = 8.0;
    assert_eq!(posts, [("ARRAY_DIMENSIONS", dims)]);
}

/// Boundary: no frame yet. `dimsPrev_` is still the constructor's zeros
/// (NDPluginDriver.cpp:111), which C never posts.
#[test]
fn no_frame_before_the_gate_posts_no_dimensions_at_the_edge() {
    let posts = array_posts_a_record_receives(
        "DIMSEDGE0",
        FFTProcessor::new(),
        &["ARRAY_DIMENSIONS"],
        Frame::None,
    );
    assert!(posts.is_empty(), "{posts:?}");
}

/// Boundary: a frame after the gate posts its dims once; the edge, which
/// came first, had none to post.
#[test]
fn a_frame_after_the_gate_has_its_dimensions_posted_once() {
    let posts = array_posts_a_record_receives(
        "DIMSEDGE2",
        FFTProcessor::new(),
        &["ARRAY_DIMENSIONS"],
        Frame::AfterTheGate(vec![1.0; 8]),
    );
    let mut dims = vec![0.0; 10];
    dims[0] = 8.0;
    assert_eq!(posts, [("ARRAY_DIMENSIONS", dims)]);
}
