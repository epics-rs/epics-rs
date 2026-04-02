use ad_core_rs::driver::{ColorMode, ImageMode};
use ad_core_rs::plugin::channel::NDArrayOutput;
use sim_detector::create_sim_detector;

#[test]
fn test_single_mode_one_frame() {
    let rt = create_sim_detector("INT_SINGLE", 64, 64, 10_000_000, NDArrayOutput::new()).unwrap();
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

    std::thread::sleep(std::time::Duration::from_millis(500));

    let acquire = handle.read_int32_blocking(rt.ad_params.acquire, 0).unwrap();
    assert_eq!(acquire, 0);
    let counter = handle
        .read_int32_blocking(rt.ad_params.base.array_counter, 0)
        .unwrap();
    assert_eq!(counter, 1);
}

#[test]
fn test_mode_switch_during_continuous() {
    let rt = create_sim_detector("INT_SWITCH", 32, 32, 10_000_000, NDArrayOutput::new()).unwrap();
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

    std::thread::sleep(std::time::Duration::from_millis(30));

    // Switch to Peaks mode
    handle
        .write_int32_blocking(rt.sim_params.sim_mode, 0, 1)
        .unwrap();

    std::thread::sleep(std::time::Duration::from_millis(30));

    // Stop
    handle
        .write_int32_blocking(rt.ad_params.acquire, 0, 0)
        .unwrap();

    std::thread::sleep(std::time::Duration::from_millis(50));

    let counter = handle
        .read_int32_blocking(rt.ad_params.base.array_counter, 0)
        .unwrap();
    assert!(
        counter >= 2,
        "should have produced frames across mode switch, got {}",
        counter
    );
}

#[test]
fn test_rgb1_mode_acquisition() {
    let rt = create_sim_detector("INT_RGB", 16, 16, 10_000_000, NDArrayOutput::new()).unwrap();
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

    std::thread::sleep(std::time::Duration::from_millis(500));

    let counter = handle
        .read_int32_blocking(rt.ad_params.base.array_counter, 0)
        .unwrap();
    assert_eq!(counter, 1);
}
