use ad_core_rs::driver::{ColorMode, ImageMode};
use ad_core_rs::ndarray::NDDataType;
use ad_core_rs::plugin::channel::NDArrayOutput;
use sim_detector::create_sim_detector;

/// Frames come on the driver's clock, which no fixed sleep bounds: Windows
/// rounds each 1 ms exposure and period wait up to its ~15.6 ms timer tick.
fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[test]
fn test_single_mode_one_frame() {
    let rt = create_sim_detector(
        &asyn_rs::registry::claim_port("INT_SINGLE").unwrap(),
        64,
        64,
        NDDataType::UInt8 as i32,
        10_000_000,
        NDArrayOutput::new(),
    )
    .unwrap();
    let handle = rt.port_handle();

    handle
        .write_int32_blocking(rt.ad_params.image_mode, 0, ImageMode::Single as i32)
        .unwrap();
    handle
        .write_float64_blocking(rt.ad_params.acquire_time, 0, 0.001)
        .unwrap();
    handle
        .write_float64_blocking(rt.ad_params.acquire_period, 0, 0.001)
        .unwrap();

    // Start
    handle
        .write_int32_blocking(rt.ad_params.acquire, 0, 1)
        .unwrap();

    let read = |idx| handle.read_int32_blocking(idx, 0).unwrap();
    wait_until("Single mode to end", || read(rt.ad_params.acquire) == 0);
    assert_eq!(read(rt.ad_params.base.array_counter), 1);
}

#[test]
fn test_mode_switch_during_continuous() {
    let rt = create_sim_detector(
        &asyn_rs::registry::claim_port("INT_SWITCH").unwrap(),
        32,
        32,
        NDDataType::UInt8 as i32,
        10_000_000,
        NDArrayOutput::new(),
    )
    .unwrap();
    let handle = rt.port_handle();

    handle
        .write_int32_blocking(rt.ad_params.image_mode, 0, ImageMode::Continuous as i32)
        .unwrap();
    handle
        .write_float64_blocking(rt.ad_params.acquire_time, 0, 0.001)
        .unwrap();
    handle
        .write_float64_blocking(rt.ad_params.acquire_period, 0, 0.002)
        .unwrap();
    // Start with LinearRamp
    handle
        .write_int32_blocking(rt.sim_params.sim_mode, 0, 0)
        .unwrap();

    // Start
    handle
        .write_int32_blocking(rt.ad_params.acquire, 0, 1)
        .unwrap();

    let counter = || {
        handle
            .read_int32_blocking(rt.ad_params.base.array_counter, 0)
            .unwrap()
    };
    wait_until("a LinearRamp frame", || counter() >= 1);

    // Switch to Peaks mode
    handle
        .write_int32_blocking(rt.sim_params.sim_mode, 0, 1)
        .unwrap();

    // The frame in flight at the switch may still be a LinearRamp one.
    let switched = counter();
    wait_until("a Peaks frame", || counter() >= switched + 2);

    // Stop
    handle
        .write_int32_blocking(rt.ad_params.acquire, 0, 0)
        .unwrap();
}

#[test]
fn test_rgb1_mode_acquisition() {
    let rt = create_sim_detector(
        &asyn_rs::registry::claim_port("INT_RGB").unwrap(),
        16,
        16,
        NDDataType::UInt8 as i32,
        10_000_000,
        NDArrayOutput::new(),
    )
    .unwrap();
    let handle = rt.port_handle();

    handle
        .write_int32_blocking(rt.ad_params.image_mode, 0, ImageMode::Single as i32)
        .unwrap();
    handle
        .write_int32_blocking(rt.ad_params.base.color_mode, 0, ColorMode::RGB1 as i32)
        .unwrap();
    handle
        .write_float64_blocking(rt.ad_params.acquire_time, 0, 0.001)
        .unwrap();
    handle
        .write_float64_blocking(rt.ad_params.acquire_period, 0, 0.001)
        .unwrap();

    // Start
    handle
        .write_int32_blocking(rt.ad_params.acquire, 0, 1)
        .unwrap();

    let read = |idx| handle.read_int32_blocking(idx, 0).unwrap();
    wait_until("Single mode to end", || read(rt.ad_params.acquire) == 0);
    assert_eq!(read(rt.ad_params.base.array_counter), 1);
    // The frame's ColorMode attribute (simDetector.cpp:171) sizes it as RGB1.
    let b = &rt.ad_params.base;
    assert_eq!(
        (
            read(b.array_size_x),
            read(b.array_size_y),
            read(b.array_size)
        ),
        (16, 16, 16 * 16 * 3)
    );
}
