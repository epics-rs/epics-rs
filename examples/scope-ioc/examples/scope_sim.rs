//! Digital oscilloscope simulator — standalone demo (no EPICS IOC).
//!
//! Demonstrates the asynPortDriver pattern using the shared driver.
//!
//! Run: `cargo run -p scope-ioc --example scope_sim`

use std::sync::Arc;

use asyn_rs::runtime::sync::Notify;

use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::create_port_runtime;
use scope_ioc::driver::*;

#[epics_base_rs::epics_main]
async fn main() {
    println!("=== asyn-rs Scope Simulator ===");
    println!("Port of EPICS testAsynPortDriver (digital oscilloscope simulator)\n");

    // 1. Create Notify + Driver
    let notify = Arc::new(Notify::new());
    let driver = ScopeSimulator::new("scopeSim", notify.clone());
    let indices = driver.param_indices();

    // 2. Create port runtime (driver moves into actor thread)
    let (runtime_handle, _jh) = create_port_runtime(driver, RuntimeConfig::default());
    let port_handle = runtime_handle.port_handle().clone();

    // 3. Subscribe for interrupt notifications
    let mut rx = port_handle.interrupts().subscribe_async();

    // 4. Spawn background simulation task
    let sim_handle = port_handle.clone();
    let sim_notify = notify.clone();
    asyn_rs::runtime::task::spawn(async move {
        sim_task_handle(sim_handle, sim_notify, indices).await;
    });

    // 5. Set update time to 0.2s and start running
    port_handle
        .write_float64(indices.p_update_time, 0, 0.2)
        .await
        .unwrap();
    port_handle
        .write_float64(indices.p_noise_amplitude, 0, 0.1)
        .await
        .unwrap();
    port_handle.write_int32(indices.p_run, 0, 1).await.unwrap();

    println!("Simulation running (1kHz sine, noise=0.1V, update=0.2s)");
    println!("Waiting for 5 waveform updates...\n");

    // 6. Receive updates
    let mut update_count = 0;
    while update_count < 5 {
        match rx.recv().await {
            Ok(iv) => {
                if iv.reason == indices.p_mean_value {
                    update_count += 1;
                    let min_v = port_handle
                        .read_float64(indices.p_min_value, 0)
                        .await
                        .unwrap_or(0.0);
                    let max_v = port_handle
                        .read_float64(indices.p_max_value, 0)
                        .await
                        .unwrap_or(0.0);
                    let mean_v = port_handle
                        .read_float64(indices.p_mean_value, 0)
                        .await
                        .unwrap_or(0.0);
                    let wf = port_handle
                        .read_float64_array(indices.p_waveform, 0, 10000)
                        .await
                        .unwrap_or_default();
                    let wf_len = wf.len();
                    println!(
                        "  Update {update_count}: waveform={wf_len} pts, \
                         min={min_v:.3}, max={max_v:.3}, mean={mean_v:.3}"
                    );
                }
            }
            Err(e) => {
                eprintln!("Receive error: {e}");
                break;
            }
        }
    }

    // 7. Change vertical gain to x10
    println!("\nSwitching vertical gain to x10...");
    let vgs_idx = port_handle
        .drv_user_create("P_VertGainSelect")
        .await
        .unwrap();
    port_handle.write_int32(vgs_idx, 0, 3).await.unwrap(); // x10

    // Receive 2 more updates
    let mut update_count = 0;
    while update_count < 2 {
        match rx.recv().await {
            Ok(iv) => {
                if iv.reason == indices.p_mean_value {
                    update_count += 1;
                    let min_v = port_handle
                        .read_float64(indices.p_min_value, 0)
                        .await
                        .unwrap_or(0.0);
                    let max_v = port_handle
                        .read_float64(indices.p_max_value, 0)
                        .await
                        .unwrap_or(0.0);
                    let mean_v = port_handle
                        .read_float64(indices.p_mean_value, 0)
                        .await
                        .unwrap_or(0.0);
                    println!(
                        "  Update (x10 gain): min={min_v:.3}, max={max_v:.3}, mean={mean_v:.3}"
                    );
                }
            }
            Err(e) => {
                eprintln!("Receive error: {e}");
                break;
            }
        }
    }

    // 8. Stop
    println!("\nStopping simulation...");
    port_handle.write_int32(indices.p_run, 0, 0).await.unwrap();

    // 9. Shutdown runtime
    runtime_handle.shutdown_and_wait();

    println!("\nDone.");
}
