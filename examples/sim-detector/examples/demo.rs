use ad_core_rs::driver::ImageMode;
use ad_core_rs::plugin::channel::NDArrayOutput;
use sim_detector::create_sim_detector;

fn main() {
    println!("SimDetector Demo");
    println!("================");

    let rt = create_sim_detector("SIM_DEMO", 256, 256, 50_000_000, NDArrayOutput::new()).unwrap();
    let handle = rt.port_handle();

    // Configure: UInt16, Continuous, LinearRamp
    handle
        .write_int32_blocking(rt.ad_params.base.data_type, 0, 3)
        .unwrap(); // UInt16
    handle
        .write_int32_blocking(rt.ad_params.image_mode, 0, ImageMode::Continuous as i32)
        .unwrap();
    handle
        .write_float64_blocking(rt.ad_params.acquire_time, 0, 0.01)
        .unwrap();
    handle
        .write_float64_blocking(rt.ad_params.acquire_period, 0, 0.05)
        .unwrap();

    // Start acquisition
    handle
        .write_int32_blocking(rt.ad_params.acquire, 0, 1)
        .unwrap();

    println!("Acquiring 10 frames...");

    // Wait and print stats
    for i in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(60));
        let counter = handle
            .read_int32_blocking(rt.ad_params.base.array_counter, 0)
            .unwrap();
        let size_x = handle
            .read_int32_blocking(rt.ad_params.base.array_size_x, 0)
            .unwrap_or(0);
        let size_y = handle
            .read_int32_blocking(rt.ad_params.base.array_size_y, 0)
            .unwrap_or(0);
        let total = handle
            .read_int32_blocking(rt.ad_params.base.array_size, 0)
            .unwrap_or(0);
        println!(
            "  Frame {}: counter={}, size={}x{}, bytes={}",
            i + 1,
            counter,
            size_x,
            size_y,
            total
        );
    }

    // Stop
    handle
        .write_int32_blocking(rt.ad_params.acquire, 0, 0)
        .unwrap();

    std::thread::sleep(std::time::Duration::from_millis(100));

    let final_count = handle
        .read_int32_blocking(rt.ad_params.base.array_counter, 0)
        .unwrap();
    println!("\nAcquisition stopped. Total frames: {}", final_count);
}
