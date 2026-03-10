//! Digital oscilloscope simulator — standalone demo (no EPICS IOC).
//!
//! Demonstrates the asynPortDriver pattern using the shared driver
//! from `asyn_rs::drivers::scope_simulator`.
//!
//! Run: `cargo run --example scope_simulator`

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Notify;

use asyn_rs::drivers::scope_simulator::*;
use asyn_rs::manager::PortManager;
use asyn_rs::port::PortDriver;
use asyn_rs::user::AsynUser;

#[tokio::main]
async fn main() {
    println!("=== asyn-rs Scope Simulator ===");
    println!("Port of EPICS testAsynPortDriver (digital oscilloscope simulator)\n");

    // 1. Create Notify + Driver
    let notify = Arc::new(Notify::new());
    let driver = ScopeSimulator::new("scopeSim", notify.clone());
    let indices = driver.param_indices();

    // 2. Register with PortManager
    let mgr = PortManager::new();
    let port = mgr.register_port(driver);

    // 3. Subscribe for interrupt notifications
    let mut rx = port.lock().base().interrupts.subscribe_async();

    // 4. Spawn background simulation task
    let sim_port: Arc<Mutex<dyn PortDriver>> = port.clone();
    let sim_notify = notify.clone();
    tokio::spawn(async move {
        sim_task_dyn(sim_port, sim_notify, indices).await;
    });

    // 5. Set update time to 0.2s and start running
    {
        let mut guard = port.lock();
        let mut user_ut = AsynUser::new(indices.p_update_time);
        guard.write_float64(&mut user_ut, 0.2).unwrap();
        let mut user_noise = AsynUser::new(indices.p_noise_amplitude);
        guard.write_float64(&mut user_noise, 0.1).unwrap();
        let mut user_run = AsynUser::new(indices.p_run);
        guard.write_int32(&mut user_run, 1).unwrap();
    }

    println!("Simulation running (1kHz sine, noise=0.1V, update=0.2s)");
    println!("Waiting for 5 waveform updates...\n");

    // 6. Receive updates
    let mut update_count = 0;
    while update_count < 5 {
        match rx.recv().await {
            Ok(iv) => {
                if iv.reason == indices.p_mean_value {
                    update_count += 1;
                    let (min_v, max_v, mean_v, wf_len) = {
                        let guard = port.lock();
                        let base = guard.base();
                        let min_v = base.params.get_float64(indices.p_min_value, 0).unwrap_or(0.0);
                        let max_v = base.params.get_float64(indices.p_max_value, 0).unwrap_or(0.0);
                        let mean_v = base.params.get_float64(indices.p_mean_value, 0).unwrap_or(0.0);
                        let wf = base.params.get_float64_array(indices.p_waveform, 0).unwrap_or_default();
                        (min_v, max_v, mean_v, wf.len())
                    };
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
    {
        let mut guard = port.lock();
        let vgs_idx = guard.base().find_param("P_VertGainSelect").unwrap();
        let mut user = AsynUser::new(vgs_idx);
        guard.write_int32(&mut user, 3).unwrap(); // x10
    }

    // Receive 2 more updates
    let mut update_count = 0;
    while update_count < 2 {
        match rx.recv().await {
            Ok(iv) => {
                if iv.reason == indices.p_mean_value {
                    update_count += 1;
                    let (min_v, max_v, mean_v) = {
                        let guard = port.lock();
                        let base = guard.base();
                        (
                            base.params.get_float64(indices.p_min_value, 0).unwrap_or(0.0),
                            base.params.get_float64(indices.p_max_value, 0).unwrap_or(0.0),
                            base.params.get_float64(indices.p_mean_value, 0).unwrap_or(0.0),
                        )
                    };
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
    {
        let mut guard = port.lock();
        let mut user = AsynUser::new(indices.p_run);
        guard.write_int32(&mut user, 0).unwrap();
    }

    // Report
    println!("\nPort report:");
    port.lock().report(1);

    println!("\nDone.");
}
