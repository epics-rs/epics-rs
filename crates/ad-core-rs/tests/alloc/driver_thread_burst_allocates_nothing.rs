//! A driver's acquisition burst on its `run_thread_named` thread allocates
//! nothing once warm. Each frame takes a queued command, replaces
//! `pArrays[0]` (simDetector.cpp:694-699) and publishes it with a `trySend`
//! (NDPluginDriver.cpp:423); with every command ready the loop never parks,
//! so its coop budget runs out mid-burst and forces a yield.
//!
//! Only the driver's thread is counted, and not its fill of the frame.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use ad_core_rs::ndarray::{NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::DriverPool;
use ad_core_rs::plugin::channel::{ArrayPublisher, NDArrayOutput, ndarray_channel};
use ad_core_rs::runtime as rt;
use alloc_count::{exempt, thread};

const DRIVER: &str = "AllocCountDet";

/// Past the 33rd frame, where the plugin queue grows the second of the two
/// 32-slot blocks it then recycles, and short of the 128 units of budget a
/// poll starts with, so the yield it forces lands in the count.
const WARM: usize = 40;
const FRAMES: usize = 512;

#[test]
fn a_driver_threads_frame_burst_allocates_nothing() {
    let (commands, mut cmd_rx) = rt::command_channel(WARM + FRAMES);
    for _ in 0..WARM + FRAMES {
        commands.try_send(()).unwrap();
    }

    let (sender, mut receiver) = ndarray_channel("SINK", 1);
    let seen = Arc::new(AtomicUsize::new(0));
    // In the driver's band: the driver spins on `yield_now`, which under FIFO
    // gives the CPU only to a thread of its own band.
    let consumer = {
        let seen = Arc::clone(&seen);
        epics_libcom_rs::runtime::task::spawn_dedicated_thread(
            "AllocCountSink".to_string(),
            rt::ThreadPriority::Medium,
            rt::StackSizeClass::Medium,
            move || {
                while let Some(frame) = receiver.blocking_recv() {
                    drop(frame);
                    seen.fetch_add(1, Ordering::Release);
                }
            },
        )
        .unwrap()
    };
    let mut output = NDArrayOutput::new();
    output.add(sender);
    let publisher = ArrayPublisher::new(Arc::new(parking_lot::Mutex::new(output)));
    let pool = DriverPool::standalone(0);
    let allocs = Arc::new(AtomicU64::new(0));
    let counted = Arc::clone(&allocs);

    rt::run_thread_named(
        DRIVER,
        rt::ThreadPriority::Medium,
        rt::StackSizeClass::Medium,
        move || async move {
            // Held across the awaits: the future never leaves this thread.
            let mut window = None;
            for i in 0..WARM + FRAMES {
                cmd_rx.recv().await.unwrap();
                if i == WARM {
                    window = Some(thread::open());
                }
                let frame = exempt(|| {
                    pool.owner()
                        .replace_array(0, || {
                            pool.alloc(
                                vec![NDDimension::new(64), NDDimension::new(64)],
                                NDDataType::UInt16,
                            )
                            .ok()
                        })
                        .unwrap()
                });
                publisher.publish(frame).await;
                // Spun, not awaited: a park would hand the loop a fresh budget.
                while seen.load(Ordering::Acquire) <= i {
                    std::thread::yield_now();
                }
            }
            counted.store(window.unwrap().close().allocs, Ordering::SeqCst);
        },
    )
    .join()
    .unwrap();
    consumer.join().unwrap();

    assert_eq!(
        allocs.load(Ordering::SeqCst),
        0,
        "{FRAMES} frames on the driver thread after {WARM} warm-up ones"
    );
}
