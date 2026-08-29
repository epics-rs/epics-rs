//! Measurement of the `NumThreads` callback pool, not a gate.
//!
//! `NDPluginDriver` runs `NumThreads` callback threads over one input queue
//! (`NDPluginDriver.cpp:996-1001`) but takes the port lock around
//! `processCallbacks` (`:509-518`), so overlap exists only for a plugin that
//! releases the lock for the expensive part — which every compute plugin in
//! ADCore does (`NDPluginStats.cpp:479`, `NDPluginROI.cpp:140`,
//! `NDPluginFFT.cpp:334`, `NDPluginProcess.cpp:139`). In this port the
//! plugin's own `Mutex` plays the role of C's port lock, so the same rule
//! decides whether the pool buys anything.
//!
//! Ignored by default because it asserts on wall clock. Run explicitly:
//!
//! ```text
//! cargo test -p ad-plugins-rs --test pool_throughput --release -- --ignored --nocapture
//! ```

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — all 1 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p ad-plugins-rs
// --all-features`, 556/556). ad-plugins-rs became a census subject when
// its `build.rs` began deriving `tokio_backend`; nothing here builds a
// CA server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.

use std::sync::Arc;

use ad_core_rs::codec::CodecName;
use ad_core_rs::ndarray::{NDArray, NDDataType, NDDimension};
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::file_base::NDFileMode;
use ad_core_rs::plugin::runtime::{
    NDPluginProcess, PluginRuntimeHandle, ProcessResult, create_plugin_runtime,
};
use ad_core_rs::plugin::wiring::WiringRegistry;
use ad_plugins_rs::bad_pixel::{BadPixel, BadPixelMode, BadPixelProcessor};
use ad_plugins_rs::codec::{CodecMode, CodecProcessor};
use ad_plugins_rs::fft::FFTProcessor;
use ad_plugins_rs::file_tiff::TiffFileProcessor;
use ad_plugins_rs::process::{ProcessConfig, ProcessProcessor};
use ad_plugins_rs::roi::create_roi_runtime;
use ad_plugins_rs::roi_stat::{ROIStatProcessor, ROIStatROI};
use ad_plugins_rs::stats::create_stats_runtime;
use ad_plugins_rs::time_series::TsReceiverRegistry;
use ad_plugins_rs::transform::{TransformProcessor, TransformType};

const WIDTH: usize = 512;
const HEIGHT: usize = 512;
const FRAMES: usize = 48;

/// Input-queue depth, deep enough that no frame can be dropped.
///
/// This is the harness's own correctness condition, not a tuning knob. A
/// plugin publishes with `blockingCallbacks=0`, which is C's `trySend`: a full
/// queue DROPS the frame and counts it in `DroppedArrays`. With a queue
/// shallower than `FRAMES` the two arms of a comparison discard different
/// numbers of frames -- the slower arm drops more, so it does less work and
/// finishes sooner -- and the ratio stops being a throughput ratio. Blocking
/// mode is not the alternative: its publish path awaits per-frame completion
/// (`channel.rs:194-208`), which serialises the producer and hides the pool
/// entirely. `drive` asserts the drop count is zero.
const QUEUE: usize = FRAMES * 4;

/// A few milliseconds of real, unshareable CPU per frame.
fn burn(array: &NDArray) -> u64 {
    let mut acc = 0u64;
    for _ in 0..96 {
        for d in array.data.as_u8_slice() {
            acc = acc.wrapping_add(*d as u64).rotate_left(1);
        }
    }
    acc
}

/// C's pattern: state is read and released, then the work runs unlocked.
struct Unlocked {
    sink: parking_lot::Mutex<u64>,
}

impl NDPluginProcess for Unlocked {
    fn process_array(&self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let acc = burn(array);
        *self.sink.lock() = acc;
        ProcessResult::empty()
    }
    fn plugin_type(&self) -> &str {
        "Unlocked"
    }
    fn does_array_callbacks(&self) -> bool {
        false
    }
}

/// The shape to avoid: one lock held across the whole body, which turns the
/// pool back into a single thread however many workers are running.
struct Locked {
    sink: parking_lot::Mutex<u64>,
}

impl NDPluginProcess for Locked {
    fn process_array(&self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let mut sink = self.sink.lock();
        *sink = burn(array);
        ProcessResult::empty()
    }
    fn plugin_type(&self) -> &str {
        "Locked"
    }
    fn does_array_callbacks(&self) -> bool {
        false
    }
}

/// Build the frames once, outside any timed region: the fill below costs about
/// as much as a plugin's own work, and generating frames inside the clock would
/// put that cost on the publisher thread where no worker can overlap it.
fn frames() -> Vec<Arc<NDArray>> {
    let mut seed = NDArray::new(
        vec![NDDimension::new(WIDTH), NDDimension::new(HEIGHT)],
        NDDataType::UInt16,
    );
    // An all-zero frame is not a workload: LZ4 finishes it in microseconds and
    // the plugin's own cost disappears under the runtime's per-frame overhead.
    let mut x = 0x2545_f491_4f6c_dd1du64;
    for i in 0..WIDTH * HEIGHT {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        seed.data.set_from_f64(i, (x % 4096) as f64);
    }
    (0..FRAMES as i32)
        .map(|id| {
            let mut arr = seed.clone();
            arr.unique_id = id;
            Arc::new(arr)
        })
        .collect()
}

fn set_threads(handle: &PluginRuntimeHandle, n: i32) {
    let port = handle.port_runtime().port_handle();
    port.write_int32_blocking(handle.plugin_params.max_threads, 0, n)
        .unwrap();
    port.write_int32_blocking(handle.plugin_params.num_threads, 0, n)
        .unwrap();
    port.write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
        .unwrap();
    assert!(handle.wait_params_applied(std::time::Duration::from_secs(30)));
    assert_eq!(
        port.read_int32_blocking(handle.plugin_params.num_threads, 0)
            .unwrap(),
        n
    );
}

/// Push `FRAMES` frames through an enabled plugin and return the wall clock
/// from the first publish to full quiescence.
fn drive(
    handle: &PluginRuntimeHandle,
    frames: &[Arc<NDArray>],
    threads: i32,
) -> std::time::Duration {
    set_threads(handle, threads);
    let port = handle.port_runtime().port_handle();
    let dropped_before = port
        .read_int32_blocking(handle.plugin_params.dropped_arrays, 0)
        .unwrap();
    let sender = handle.array_sender().clone();
    let frames: Vec<Arc<NDArray>> = frames.to_vec();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let t0 = std::time::Instant::now();
    rt.block_on(async {
        for f in frames {
            sender.publish(f).await;
        }
    });
    assert!(handle.wait_params_applied(std::time::Duration::from_secs(60)));
    let elapsed = t0.elapsed();
    let dropped = port
        .read_int32_blocking(handle.plugin_params.dropped_arrays, 0)
        .unwrap()
        - dropped_before;
    assert_eq!(
        dropped, 0,
        "NumThreads={threads}: {dropped} of {FRAMES} frames were dropped, so this \
         timing is not a throughput measurement"
    );
    elapsed
}

fn report(what: &str, one: std::time::Duration, four: std::time::Duration) -> f64 {
    let speedup = one.as_secs_f64() / four.as_secs_f64();
    println!(
        "{what:28} NumThreads=1 {:>8.0} ms   NumThreads=4 {:>8.0} ms   speedup {speedup:.2}x",
        one.as_secs_f64() * 1000.0,
        four.as_secs_f64() * 1000.0
    );
    speedup
}

#[test]
#[ignore = "wall-clock measurement"]
fn measure_pool_throughput() {
    let wiring = Arc::new(WiringRegistry::new());
    let pool = Arc::new(NDArrayPool::new(64_000_000));
    let frames = frames();

    let (unlocked, _jh1) = create_plugin_runtime(
        "POOL_UNLOCKED",
        Unlocked {
            sink: parking_lot::Mutex::new(0),
        },
        pool.clone(),
        QUEUE,
        "",
        wiring.clone(),
    );
    drive(&unlocked, &frames, 1); // warm up: thread spawn, first-touch page faults
    let u1 = drive(&unlocked, &frames, 1);
    let u4 = drive(&unlocked, &frames, 4);

    let (locked, _jh2) = create_plugin_runtime(
        "POOL_LOCKED",
        Locked {
            sink: parking_lot::Mutex::new(0),
        },
        pool.clone(),
        QUEUE,
        "",
        wiring.clone(),
    );
    drive(&locked, &frames, 1);
    let l1 = drive(&locked, &frames, 1);
    let l4 = drive(&locked, &frames, 4);

    let unlocked_speedup = report("lock released for work", u1, u4);
    let locked_speedup = report("lock held across work", l1, l4);

    assert!(
        unlocked_speedup > 2.0,
        "four callback threads must overlap a plugin that releases its lock"
    );
    assert!(
        locked_speedup < 1.5,
        "a plugin holding one lock across process_array cannot overlap"
    );
}

/// The same measurement against the shipped plugins, one row per plugin whose
/// `process_array` does enough work for the pool to matter. `NDPluginStats` is
/// the control: it already copies its config out of the lock and computes
/// unlocked, exactly as C does at `NDPluginStats.cpp:479`.
#[test]
#[ignore = "wall-clock measurement"]
fn measure_shipped_plugin_throughput() {
    let wiring = Arc::new(WiringRegistry::new());
    let pool = Arc::new(NDArrayPool::new(512_000_000));
    let ts_registry = TsReceiverRegistry::new();
    let mut rows: Vec<(&str, std::time::Duration, std::time::Duration)> = Vec::new();
    let frames = frames();

    // Control: C releases at NDPluginStats.cpp:479, and so do we.
    let (stats, _sink, stats_params, _j) = create_stats_runtime(
        "POOL_STATS",
        pool.clone(),
        QUEUE,
        "",
        wiring.clone(),
        &ts_registry,
    );
    let sp = stats.port_runtime().port_handle();
    sp.write_int32_blocking(stats_params.compute_statistics, 0, 1)
        .unwrap();
    sp.write_int32_blocking(stats_params.compute_centroid, 0, 1)
        .unwrap();
    sp.write_int32_blocking(stats_params.compute_profiles, 0, 1)
        .unwrap();
    drive(&stats, &frames, 1);
    rows.push((
        "Stats  (control)",
        drive(&stats, &frames, 1),
        drive(&stats, &frames, 4),
    ));

    // C releases at NDPluginROI.cpp:140.
    let (roi, roi_params, _j) =
        create_roi_runtime("POOL_ROI", pool.clone(), QUEUE, "", wiring.clone());
    let rp = roi.port_runtime().port_handle();
    // An unconfigured ROI extracts nothing, which measures nothing.
    for (dim, extent) in [(0, WIDTH), (1, HEIGHT)] {
        rp.write_int32_blocking(roi_params.dims[dim].enable, 0, 1)
            .unwrap();
        rp.write_int32_blocking(roi_params.dims[dim].size, 0, extent as i32)
            .unwrap();
    }
    drive(&roi, &frames, 1);
    rows.push(("ROI", drive(&roi, &frames, 1), drive(&roi, &frames, 4)));

    // C releases at NDPluginROIStat.cpp:267.
    let quadrant = |x: usize, y: usize| ROIStatROI {
        enabled: true,
        offset: [x, y],
        size: [WIDTH / 2, HEIGHT / 2],
        bgd_width: 0,
    };
    let (roi_stat, _j) = create_plugin_runtime(
        "POOL_ROISTAT",
        ROIStatProcessor::new(
            vec![
                quadrant(0, 0),
                quadrant(WIDTH / 2, 0),
                quadrant(0, HEIGHT / 2),
                quadrant(WIDTH / 2, HEIGHT / 2),
            ],
            16,
        ),
        pool.clone(),
        QUEUE,
        "",
        wiring.clone(),
    );
    drive(&roi_stat, &frames, 1);
    rows.push((
        "ROIStat",
        drive(&roi_stat, &frames, 1),
        drive(&roi_stat, &frames, 4),
    ));

    // C releases around every codec call (NDPluginCodec.cpp:556, :596).
    let (codec, _j) = create_plugin_runtime(
        "POOL_CODEC",
        // Zlib rather than LZ4: LZ4 finishes a frame this size in well under
        // the harness's resolution, so the row would report noise.
        CodecProcessor::new(CodecMode::Compress {
            codec: CodecName::Zlib,
            quality: 85,
        }),
        pool.clone(),
        QUEUE,
        "",
        wiring.clone(),
    );
    drive(&codec, &frames, 1);
    rows.push((
        "Codec (zlib)",
        drive(&codec, &frames, 1),
        drive(&codec, &frames, 4),
    ));

    // C releases at NDPluginBadPixel.cpp:233.
    let pixels: Vec<BadPixel> = (0..4096)
        .map(|i| BadPixel {
            x: (i * 7 % WIDTH) as i64,
            y: (i * 13 % HEIGHT) as i64,
            mode: BadPixelMode::Median {
                half_x: 2,
                half_y: 2,
            },
        })
        .collect();
    let (bad_pixel, _j) = create_plugin_runtime(
        "POOL_BADPIX",
        BadPixelProcessor::new(pixels),
        pool.clone(),
        QUEUE,
        "",
        wiring.clone(),
    );
    drive(&bad_pixel, &frames, 1);
    rows.push((
        "BadPixel",
        drive(&bad_pixel, &frames, 1),
        drive(&bad_pixel, &frames, 4),
    ));

    // C releases at NDPluginTransform.cpp:500.
    let (transform, _j) = create_plugin_runtime(
        "POOL_TRANSFORM",
        TransformProcessor::new(TransformType::Rot90CW),
        pool.clone(),
        QUEUE,
        "",
        wiring.clone(),
    );
    drive(&transform, &frames, 1);
    rows.push((
        "Transform",
        drive(&transform, &frames, 1),
        drive(&transform, &frames, 4),
    ));

    // C releases at NDPluginFFT.cpp:334.
    let (fft, _j) = create_plugin_runtime(
        "POOL_FFT",
        FFTProcessor::new(),
        pool.clone(),
        QUEUE,
        "",
        wiring.clone(),
    );
    drive(&fft, &frames, 1);
    rows.push(("FFT", drive(&fft, &frames, 1), drive(&fft, &frames, 4)));

    // C releases at NDPluginProcess.cpp:139.
    let (process, _j) = create_plugin_runtime(
        "POOL_PROCESS",
        ProcessProcessor::new(ProcessConfig {
            enable_offset_scale: true,
            offset: -1.0,
            scale: 2.0,
            enable_high_clip: true,
            high_clip_thresh: 60000.0,
            high_clip_value: 60000.0,
            ..ProcessConfig::default()
        }),
        pool.clone(),
        QUEUE,
        "",
        wiring.clone(),
    );
    drive(&process, &frames, 1);
    rows.push((
        "Process",
        drive(&process, &frames, 1),
        drive(&process, &frames, 4),
    ));

    // A file plugin, for contrast. Its `ctrl` mutex is C's `fileMutexId`, which
    // C takes around `writeFile` (NDPluginFile.cpp:247-250) and holds for the
    // whole write while only the port lock drops -- so this row is SUPPOSED to
    // sit in the held regime, and it is here to keep that visible rather than
    // to be fixed.
    let dir = std::env::temp_dir().join(format!("ad_pool_tiff_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let tiff = TiffFileProcessor::new();
    {
        let mut c = tiff.ctrl.lock();
        c.auto_save = true;
        c.file_base.file_path = format!("{}/", dir.display());
        c.file_base.file_name = "pool".into();
        c.file_base.file_template = "%s%s_%3.3d.tif".into();
        c.file_base.auto_increment = true;
        c.file_base.set_mode(NDFileMode::Single);
    }
    let (tiff, _j) =
        create_plugin_runtime("POOL_TIFF", tiff, pool.clone(), QUEUE, "", wiring.clone());
    drive(&tiff, &frames, 1);
    let held = (
        "TIFF file (ctrl held)",
        drive(&tiff, &frames, 1),
        drive(&tiff, &frames, 4),
    );
    std::fs::remove_dir_all(&dir).ok();

    for &(what, one, four) in &rows {
        report(what, one, four);
    }
    let held_speedup = report(held.0, held.1, held.2);

    // Every shipped plugin now releases its lock before the expensive part, as
    // C does, so every row must beat one thread. A plugin that re-acquires the
    // hold lands in the other regime entirely -- the held-lock rows measured
    // 0.50-0.99x before this was fixed, against 1.98x for the worst released
    // one -- so the threshold sits between the two regimes, not near either.
    // The file plugin holds deliberately, so it must NOT be in the released
    // regime — if this ever crosses, the `ctrl` lock stopped covering the write
    // and the C `fileMutexId` parity claim above is no longer true.
    assert!(
        held_speedup < 1.2,
        "{}: {held_speedup:.2}x — the file plugin's ctrl lock no longer spans the write",
        held.0
    );

    for &(what, one, four) in &rows {
        let speedup = one.as_secs_f64() / four.as_secs_f64();
        assert!(
            speedup > 1.2,
            "{what}: NumThreads=4 is {speedup:.2}x of NumThreads=1 -- \
             the plugin is holding its lock across the work"
        );
    }
}
