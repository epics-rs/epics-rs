// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — all 1 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p ad-core-rs
// --all-features`, 345/345). ad-core-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.
use std::sync::Arc;

use asyn_rs::error::AsynResult;
use asyn_rs::port::{PortDriverBase, PortFlags};

use crate::driver::ndarray_driver::{init_read_only_params, refresh_pool_stats};
use crate::ndarray::NDArray;
use crate::ndarray_pool::NDArrayPool;
use crate::params::ad_driver::ADDriverParams;
use crate::plugin::channel::{NDArrayOutput, NDArraySender, QueuedArrayCounter};

use super::{ADStatus, ShutterMode};

/// Base state for ADDriver (extends NDArrayDriver with detector-specific params).
pub struct ADDriverBase {
    pub port_base: PortDriverBase,
    pub params: ADDriverParams,
    pub pool: Arc<NDArrayPool>,
    pub array_output: NDArrayOutput,
    pub queued_counter: Arc<QueuedArrayCounter>,
    /// Most recently prepared array (C++ `pArrays[0]`), used as the template
    /// for `preAllocateBuffers`.
    pub last_array: Option<Arc<NDArray>>,
}

impl ADDriverBase {
    pub fn new(
        port_name: &str,
        max_size_x: i32,
        max_size_y: i32,
        max_memory: usize,
    ) -> AsynResult<Self> {
        let mut port_base = PortDriverBase::new(
            port_name,
            1,
            PortFlags {
                can_block: true,
                ..Default::default()
            },
        );

        let params = ADDriverParams::create(&mut port_base)?;
        let pool = Arc::new(NDArrayPool::new(max_memory));

        // C `ADDriver::ADDriver` (ADDriver.cpp:150) chains to
        // `asynNDArrayDriver::asynNDArrayDriver`, so its whole read-only block
        // runs for every AD driver before the detector seeds below.
        init_read_only_params(&mut port_base, &params.base, port_name)?;
        refresh_pool_stats(&mut port_base, &params.base, &pool)?;
        // Set initial values
        port_base.set_string_param(params.base.codec, 0, String::new())?;

        // ADDriver.cpp:174-179 states the rule these seeds follow: "values set
        // here will override those in the database for output records because if
        // asyn device support reads a value from the driver with no error during
        // initialization then it sets the output record to that value."
        //
        // So the constructor seeds ONLY read-only parameters and the read/write
        // ones that must not come from the DB (ADDriver.cpp:180-191). ADGain,
        // ADTemperature, ADBinX/Y, ADSizeX/Y, ADImageMode, ADNumImages,
        // ADNumExposures, ADAcquireTime/Period and ADShutterMode are deliberately
        // absent: their records carry PINI="YES" with a DB default and are
        // restored by autosave. Seeding them here would clobber both.
        port_base.set_int32_param(params.max_size_x, 0, max_size_x)?;
        port_base.set_int32_param(params.max_size_y, 0, max_size_y)?;
        port_base.set_int32_param(params.status, 0, ADStatus::Idle as i32)?;
        port_base.set_int32_param(params.num_images_counter, 0, 0)?;
        port_base.set_int32_param(params.num_exposures_counter, 0, 0)?;
        port_base.set_float64_param(params.time_remaining, 0, 0.0)?;
        port_base.set_int32_param(params.shutter_status, 0, 0)?;
        port_base.set_int32_param(params.acquire, 0, 0)?;
        port_base.set_string_param(params.status_message, 0, String::new())?;
        port_base.set_string_param(params.string_to_server, 0, String::new())?;
        port_base.set_string_param(params.string_from_server, 0, String::new())?;

        // Not in C's block, which leaves both to the database: this port has
        // no NDDataType_RBV default to fall back on, and a detector with no
        // downstream plugins is still expected to publish arrays.
        port_base.set_int32_param(params.base.data_type, 0, 1)?; // UInt8
        port_base.set_int32_param(params.base.array_callbacks, 0, 1)?;

        // Don't fire callbacks here — no record subscribers exist before
        // dbLoadRecords, so an early `call_param_callbacks(0)` only
        // consumes the change flags silently. The flags must stay
        // pending until the first post-iocInit `call_param_callbacks`
        // (acquire start / poll / write) flushes them to the now-
        // registered I/O Intr subscribers. The seeds above then reach
        // MaxSizeX_RBV / Status_RBV / etc with a real timestamp; the
        // parameters left unseeded arrive from their PINI="YES" records
        // (or autosave) at iocInit, as in C.

        Ok(Self {
            port_base,
            params,
            pool,
            array_output: NDArrayOutput::new(),
            queued_counter: Arc::new(QueuedArrayCounter::new()),
            last_array: None,
        })
    }

    /// Connect a downstream channel-based receiver.
    pub fn connect_downstream(&mut self, mut sender: NDArraySender) {
        sender.set_queued_counter(self.queued_counter.clone());
        self.array_output.add(sender);
    }

    /// Publish an array: update counters, push to plugins and channel outputs, fire callbacks.
    /// Updates driver param cache and fires param callbacks for a new array.
    /// If array callbacks are enabled, returns the array that the caller must
    /// publish asynchronously to downstream consumers via
    /// `array_output.publish(arr).await`.
    ///
    /// This function does NOT publish the array — the caller is responsible
    /// for that in an async context. Returns `None` when callbacks are disabled.
    pub fn prepare_array(&mut self, array: Arc<NDArray>) -> AsynResult<Option<Arc<NDArray>>> {
        let counter = self
            .port_base
            .get_int32_param(self.params.base.array_counter, 0)?
            + 1;
        self.port_base
            .set_int32_param(self.params.base.array_counter, 0, counter)?;

        // G5/G6/G7: write all per-array parameters (size, dims, type, color,
        // Bayer, timestamps, codec).
        crate::driver::ndarray_driver::write_array_params(
            &mut self.port_base,
            &self.params.base,
            &array,
        )?;

        // Record this as the template array for preAllocateBuffers.
        self.last_array = Some(array.clone());

        // Update pool stats
        self.port_base.set_float64_param(
            self.params.base.pool_used_memory,
            0,
            self.pool.allocated_bytes() as f64 / 1_048_576.0,
        )?;
        self.port_base.set_int32_param(
            self.params.base.pool_free_buffers,
            0,
            self.pool.num_free_buffers() as i32,
        )?;
        self.port_base.set_int32_param(
            self.params.base.pool_alloc_buffers,
            0,
            self.pool.num_alloc_buffers() as i32,
        )?;

        let callbacks_enabled = self
            .port_base
            .get_int32_param(self.params.base.array_callbacks, 0)?
            != 0;

        let to_publish = if callbacks_enabled {
            self.port_base.set_generic_pointer_param(
                self.params.base.ndarray_data,
                0,
                array.clone() as Arc<dyn std::any::Any + Send + Sync>,
            )?;
            Some(array)
        } else {
            None
        };

        self.port_base.call_param_callbacks(0)?;

        Ok(to_publish)
    }

    /// Set shutter state (open/close).
    ///
    /// Mirrors C++ `ADDriver::setShutter` (ADDriver.cpp:29-52):
    /// - `ADShutterModeNone`: no-op.
    /// - `ADShutterModeEPICS`: write `ADShutterControlEPICS`, fire callbacks,
    ///   then sleep `shutterOpenDelay - shutterCloseDelay`.
    /// - `ADShutterModeDetector`: empty break — detector drivers override
    ///   `setShutter` themselves.
    ///
    /// C++ never writes `ADShutterStatus` here — the actual shutter status is
    /// driven by the shutter hardware / EPICS records, not assumed.
    pub fn set_shutter(&mut self, open: bool) -> AsynResult<()> {
        let mode = ShutterMode::from_i32(
            self.port_base
                .get_int32_param(self.params.shutter_mode, 0)?,
        );

        match mode {
            Some(ShutterMode::None) | None => {}
            Some(ShutterMode::DetectorOnly) => {
                // C++ ADShutterModeDetector is an empty break; detector drivers
                // override setShutter. Nothing to do in the base implementation.
            }
            Some(ShutterMode::EpicsOnly) => {
                let open_delay = self
                    .port_base
                    .get_float64_param(self.params.shutter_open_delay, 0)?;
                let close_delay = self
                    .port_base
                    .get_float64_param(self.params.shutter_close_delay, 0)?;
                self.port_base.set_int32_param(
                    self.params.shutter_control_epics,
                    0,
                    if open { 1 } else { 0 },
                )?;
                self.port_base.call_param_callbacks(0)?;
                // C++: epicsThreadSleep(shutterOpenDelay - shutterCloseDelay).
                let delay = open_delay - close_delay;
                if delay > 0.0 {
                    std::thread::sleep(epics_libcom_rs::runtime::time::duration_from_secs(delay));
                }
            }
        }

        Ok(())
    }

    /// Handle a write to a pool-control Int32 parameter (`POOL_EMPTY_FREELIST`,
    /// `POOL_POLL_STATS`, `POOL_PRE_ALLOC_BUFFERS`), mirroring the pool branch
    /// of C++ `asynNDArrayDriver::writeInt32`.
    ///
    /// Returns `true` when `param_index` was a recognized pool-control
    /// parameter.
    pub fn write_int32_pool(&mut self, param_index: usize, _value: i32) -> AsynResult<bool> {
        let template = self.last_array.clone();
        crate::driver::ndarray_driver::handle_pool_write_int32(
            &mut self.port_base,
            &self.params.base,
            &self.pool,
            param_index,
            template.as_deref(),
        )
    }

    /// Write `ADAcquire` and drive `ADAcquireBusy` accordingly.
    ///
    /// Mirrors the `ADAcquire` branch of C++ `asynNDArrayDriver::setIntegerParam`
    /// (asynNDArrayDriver.cpp:636-663):
    /// - `value != 0` (start): `ADAcquireBusy = 1`.
    /// - `value == 0` (stop): `ADAcquireBusy = 0`, but when `ADWaitForPlugins`
    ///   is set, only once the live queued-array count has reached 0.
    ///
    /// `ADAcquire` itself is always written to `value`.
    pub fn set_acquire(&mut self, value: i32) -> AsynResult<()> {
        if value == 0 {
            let wait_for_plugins = self
                .port_base
                .get_int32_param(self.params.wait_for_plugins, 0)
                .unwrap_or(0)
                != 0;
            if !wait_for_plugins || self.queued_counter.get() == 0 {
                self.port_base
                    .set_int32_param(self.params.acquire_busy, 0, 0)?;
            }
        } else {
            self.port_base
                .set_int32_param(self.params.acquire_busy, 0, 1)?;
        }
        self.port_base
            .set_int32_param(self.params.acquire, 0, value)?;
        Ok(())
    }

    /// Write `NDNumQueuedArrays` and clear `ADAcquireBusy` when the queue
    /// drains while acquisition has already stopped.
    ///
    /// Mirrors the `NDNumQueuedArrays` branch of C++
    /// `asynNDArrayDriver::setIntegerParam`: when `NDNumQueuedArrays` reaches 0
    /// and `ADAcquire == 0`, `ADAcquireBusy` is set to 0.
    pub fn set_num_queued_arrays(&mut self, value: i32) -> AsynResult<()> {
        if value == 0 {
            let acquire = self
                .port_base
                .get_int32_param(self.params.acquire, 0)
                .unwrap_or(0);
            if acquire == 0 {
                self.port_base
                    .set_int32_param(self.params.acquire_busy, 0, 0)?;
            }
        }
        self.port_base
            .set_int32_param(self.params.base.num_queued_arrays, 0, value)?;
        Ok(())
    }
}

/// Trait for areaDetector drivers.
pub trait ADDriver: asyn_rs::port::PortDriver {
    fn ad_base(&self) -> &ADDriverBase;
    fn ad_base_mut(&mut self) -> &mut ADDriverBase;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::ndarray_driver::tests::{assert_c_read_only_block, assert_fresh_pool_stats};

    #[test]
    fn test_constructor_seeds_the_c_read_only_block() {
        // C++ `ADDriver::ADDriver` (ADDriver.cpp:150) chains to
        // `asynNDArrayDriver::asynNDArrayDriver`, so the whole of
        // asynNDArrayDriver.cpp:954-1005 must be readable on a detector port
        // too, not just the six seeds this constructor used to repeat.
        let ad = ADDriverBase::new("TEST", 1024, 768, 50_000_000).unwrap();
        assert_c_read_only_block(&ad.port_base, &ad.params.base, "TEST");
        assert_fresh_pool_stats(&ad.port_base, &ad.params.base, 50_000_000.0 / 1_048_576.0);
    }

    #[test]
    fn test_new_sets_initial_params() {
        let ad = ADDriverBase::new("TEST", 1024, 768, 50_000_000).unwrap();
        // C++ ADDriver.cpp:180-191 — the full set of parameters the constructor
        // seeds, and nothing more.
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.max_size_x, 0)
                .unwrap(),
            1024
        );
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.max_size_y, 0)
                .unwrap(),
            768
        );
        assert_eq!(
            ad.port_base.get_int32_param(ad.params.status, 0).unwrap(),
            ADStatus::Idle as i32
        );
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.num_images_counter, 0)
                .unwrap(),
            0
        );
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.num_exposures_counter, 0)
                .unwrap(),
            0
        );
        assert_eq!(
            ad.port_base
                .get_float64_param(ad.params.time_remaining, 0)
                .unwrap(),
            0.0
        );
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.shutter_status, 0)
                .unwrap(),
            0
        );
        assert_eq!(
            ad.port_base.get_int32_param(ad.params.acquire, 0).unwrap(),
            0
        );
    }

    #[test]
    fn test_r6_70_constructor_leaves_db_owned_params_unseeded() {
        // R6-70 / ADDriver.cpp:174-179 — "values set here will override those in
        // the database for output records". ADGain, ADTemperature, ADBinX/Y,
        // ADSizeX/Y, ADImageMode, ADNumImages, ADNumExposures, ADAcquireTime/
        // Period and ADShutterMode all carry PINI="YES" DB defaults (and are
        // restored by autosave), so the constructor must NOT seed them.
        // ADStatusMessage is seeded, but to "" (:189) — not "Idle".
        let ad = ADDriverBase::new("TEST", 1024, 768, 50_000_000).unwrap();

        for (name, idx) in [
            ("SizeX", ad.params.size_x),
            ("SizeY", ad.params.size_y),
            ("BinX", ad.params.bin_x),
            ("BinY", ad.params.bin_y),
            ("ImageMode", ad.params.image_mode),
            ("NumImages", ad.params.num_images),
            ("NumExposures", ad.params.num_exposures),
            ("ShutterMode", ad.params.shutter_mode),
        ] {
            assert_eq!(
                ad.port_base.get_int32_param(idx, 0).unwrap(),
                0,
                "{name} must be left to the database, not seeded by the driver"
            );
        }
        for (name, idx) in [
            ("Gain", ad.params.gain),
            ("Temperature", ad.params.temperature),
            ("TemperatureActual", ad.params.temperature_actual),
            ("AcquireTime", ad.params.acquire_time),
            ("AcquirePeriod", ad.params.acquire_period),
        ] {
            assert_eq!(
                ad.port_base.get_float64_param(idx, 0).unwrap(),
                0.0,
                "{name} must be left to the database, not seeded by the driver"
            );
        }

        assert_eq!(
            ad.port_base
                .get_string_param(ad.params.status_message, 0)
                .unwrap(),
            b"",
            "ADStatusMessage is seeded to the empty string (ADDriver.cpp:189)"
        );
        assert_eq!(
            ad.port_base
                .get_string_param(ad.params.string_to_server, 0)
                .unwrap(),
            b""
        );
        assert_eq!(
            ad.port_base
                .get_string_param(ad.params.string_from_server, 0)
                .unwrap(),
            b""
        );
    }

    #[test]
    fn test_prepare_array_increments_counter() {
        let mut ad = ADDriverBase::new("TEST", 256, 256, 50_000_000).unwrap();
        let arr = ad
            .pool
            .alloc(
                vec![
                    crate::ndarray::NDDimension::new(256),
                    crate::ndarray::NDDimension::new(256),
                ],
                crate::ndarray::NDDataType::UInt8,
            )
            .unwrap();
        ad.prepare_array(Arc::new(arr)).unwrap();
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.base.array_counter, 0)
                .unwrap(),
            1
        );
    }

    #[test]
    fn test_prepare_array_skips_output_when_callbacks_disabled() {
        use crate::plugin::channel::ndarray_channel;

        let mut ad = ADDriverBase::new("TEST", 64, 64, 1_000_000).unwrap();
        let (sender, _receiver) = ndarray_channel("DOWNSTREAM", 10);
        ad.connect_downstream(sender);

        ad.port_base
            .set_int32_param(ad.params.base.array_callbacks, 0, 0)
            .unwrap();

        let arr = ad
            .pool
            .alloc(
                vec![
                    crate::ndarray::NDDimension::new(64),
                    crate::ndarray::NDDimension::new(64),
                ],
                crate::ndarray::NDDataType::UInt8,
            )
            .unwrap();
        ad.prepare_array(Arc::new(arr)).unwrap();

        // Counter still increments, but generic pointer should NOT be updated to an NDArray
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.base.array_counter, 0)
                .unwrap(),
            1
        );
        // Generic pointer should still be the default (unit type), not an NDArray
        let gp = ad
            .port_base
            .get_generic_pointer_param(ad.params.base.ndarray_data, 0)
            .unwrap();
        assert!(gp.downcast_ref::<NDArray>().is_none());
    }

    #[test]
    fn test_publish_sets_generic_pointer() {
        let mut ad = ADDriverBase::new("TEST", 8, 8, 1_000_000).unwrap();
        let arr = ad
            .pool
            .alloc(
                vec![
                    crate::ndarray::NDDimension::new(8),
                    crate::ndarray::NDDimension::new(8),
                ],
                crate::ndarray::NDDataType::UInt8,
            )
            .unwrap();
        let id = arr.unique_id;
        ad.prepare_array(Arc::new(arr)).unwrap();

        let gp = ad
            .port_base
            .get_generic_pointer_param(ad.params.base.ndarray_data, 0)
            .unwrap();
        let recovered = gp.downcast_ref::<NDArray>().unwrap();
        assert_eq!(recovered.unique_id, id);
    }

    #[test]
    fn test_shutter_detector_mode_is_noop_in_base() {
        // C parity: ADShutterModeDetector is an empty break in the base driver;
        // detector drivers override setShutter themselves. The base must NOT
        // touch SHUTTER_CONTROL or SHUTTER_STATUS.
        let mut ad = ADDriverBase::new("TEST", 8, 8, 1_000_000).unwrap();
        ad.port_base
            .set_int32_param(ad.params.shutter_mode, 0, ShutterMode::DetectorOnly as i32)
            .unwrap();
        ad.port_base
            .set_int32_param(ad.params.shutter_control, 0, 7)
            .unwrap();

        ad.set_shutter(true).unwrap();
        // Unchanged — base setShutter does nothing for detector mode.
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.shutter_control, 0)
                .unwrap(),
            7
        );
    }

    #[test]
    fn test_shutter_control_epics_mode() {
        let mut ad = ADDriverBase::new("TEST", 8, 8, 1_000_000).unwrap();
        ad.port_base
            .set_int32_param(ad.params.shutter_mode, 0, ShutterMode::EpicsOnly as i32)
            .unwrap();

        ad.set_shutter(true).unwrap();
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.shutter_control_epics, 0)
                .unwrap(),
            1
        );
        // C parity: setShutter never writes SHUTTER_STATUS. The constructor
        // seeds it to 0 (ADDriver.cpp:186) and nothing in setShutter moves it —
        // the detector driver reports the real shutter state.
        assert_eq!(
            ad.port_base
                .get_int32_param_strict(ad.params.shutter_status, 0)
                .ok(),
            Some(0),
            "SHUTTER_STATUS must keep its constructor value through setShutter"
        );
    }

    #[test]
    fn test_shutter_epics_mode_sleeps_open_minus_close() {
        // C parity: EPICS mode sleeps (shutterOpenDelay - shutterCloseDelay).
        let mut ad = ADDriverBase::new("TEST", 8, 8, 1_000_000).unwrap();
        ad.port_base
            .set_int32_param(ad.params.shutter_mode, 0, ShutterMode::EpicsOnly as i32)
            .unwrap();
        ad.port_base
            .set_float64_param(ad.params.shutter_open_delay, 0, 0.05)
            .unwrap();
        ad.port_base
            .set_float64_param(ad.params.shutter_close_delay, 0, 0.01)
            .unwrap();

        let start = std::time::Instant::now();
        ad.set_shutter(true).unwrap();
        let elapsed = start.elapsed();
        // delay = 0.05 - 0.01 = 0.04 s.
        assert!(
            elapsed >= std::time::Duration::from_millis(35),
            "expected ~40ms sleep, got {elapsed:?}"
        );
    }

    #[test]
    fn test_set_acquire_drives_acquire_busy() {
        // G8: ADAcquire=1 sets AcquireBusy=1; ADAcquire=0 (no WaitForPlugins)
        // immediately clears AcquireBusy.
        let mut ad = ADDriverBase::new("TEST", 8, 8, 1_000_000).unwrap();
        ad.set_acquire(1).unwrap();
        assert_eq!(
            ad.port_base.get_int32_param(ad.params.acquire, 0).unwrap(),
            1
        );
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.acquire_busy, 0)
                .unwrap(),
            1
        );
        ad.set_acquire(0).unwrap();
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.acquire_busy, 0)
                .unwrap(),
            0
        );
    }

    #[test]
    fn test_set_acquire_wait_for_plugins_gating() {
        // G8: with WaitForPlugins set and queued arrays outstanding, ADAcquire=0
        // does NOT clear AcquireBusy until NDNumQueuedArrays drains to 0.
        let mut ad = ADDriverBase::new("TEST", 8, 8, 1_000_000).unwrap();
        ad.port_base
            .set_int32_param(ad.params.wait_for_plugins, 0, 1)
            .unwrap();
        ad.set_acquire(1).unwrap();
        ad.queued_counter.increment(); // one array still queued

        ad.set_acquire(0).unwrap();
        // Still busy — queue not drained.
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.acquire_busy, 0)
                .unwrap(),
            1
        );

        // Drain the queue, then NDNumQueuedArrays=0 clears AcquireBusy.
        ad.queued_counter.decrement();
        ad.set_num_queued_arrays(0).unwrap();
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.acquire_busy, 0)
                .unwrap(),
            0
        );
    }

    #[test]
    fn test_write_int32_pool_empty_free_list() {
        // G3: writing POOL_EMPTY_FREELIST empties the free list.
        let mut ad = ADDriverBase::new("TEST", 8, 8, 10_000_000).unwrap();
        let arr = ad
            .pool
            .alloc(
                vec![crate::ndarray::NDDimension::new(100)],
                crate::ndarray::NDDataType::UInt8,
            )
            .unwrap();
        ad.pool.release(arr);
        assert_eq!(ad.pool.num_free_buffers(), 1);

        let handled = ad
            .write_int32_pool(ad.params.base.pool_empty_free_list, 1)
            .unwrap();
        assert!(handled);
        assert_eq!(ad.pool.num_free_buffers(), 0);
    }

    #[test]
    fn test_write_int32_pool_pre_alloc() {
        // G3: writing POOL_PRE_ALLOC_BUFFERS pre-allocates from the last array.
        let mut ad = ADDriverBase::new("TEST", 8, 8, 10_000_000).unwrap();
        let arr = ad
            .pool
            .alloc(
                vec![crate::ndarray::NDDimension::new(64)],
                crate::ndarray::NDDataType::UInt8,
            )
            .unwrap();
        ad.prepare_array(Arc::new(arr)).unwrap();
        ad.port_base
            .set_int32_param(ad.params.base.pool_num_pre_alloc_buffers, 0, 3)
            .unwrap();

        let handled = ad
            .write_int32_pool(ad.params.base.pool_pre_alloc, 1)
            .unwrap();
        assert!(handled);
        // The 3 pre-allocated buffers land on the free list.
        assert_eq!(ad.pool.num_free_buffers(), 3);
        // C parity: POOL_PRE_ALLOC_BUFFERS is reset to 0 after running.
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.base.pool_pre_alloc, 0)
                .unwrap(),
            0
        );
    }

    #[test]
    fn test_write_int32_pool_unrecognized() {
        // G3: a non-pool parameter is not handled.
        let mut ad = ADDriverBase::new("TEST", 8, 8, 1_000_000).unwrap();
        let handled = ad.write_int32_pool(ad.params.gain, 1).unwrap();
        assert!(!handled);
    }

    #[test]
    fn test_gain_and_temperature() {
        // The constructor leaves both to the database (ADDriver.cpp:174-179);
        // once written, they read back through the parameter library.
        let mut ad = ADDriverBase::new("TEST", 8, 8, 1_000_000).unwrap();
        ad.port_base
            .set_float64_param(ad.params.gain, 0, 1.0)
            .unwrap();
        ad.port_base
            .set_float64_param(ad.params.temperature, 0, 25.0)
            .unwrap();
        assert_eq!(
            ad.port_base.get_float64_param(ad.params.gain, 0).unwrap(),
            1.0
        );
        assert_eq!(
            ad.port_base
                .get_float64_param(ad.params.temperature, 0)
                .unwrap(),
            25.0
        );
    }

    #[test]
    fn test_connect_downstream() {
        use crate::plugin::channel::ndarray_channel;

        let mut ad = ADDriverBase::new("TEST", 8, 8, 1_000_000).unwrap();
        let (sender, mut receiver) = ndarray_channel("DOWNSTREAM", 10);
        ad.connect_downstream(sender);

        let arr = ad
            .pool
            .alloc(
                vec![
                    crate::ndarray::NDDimension::new(8),
                    crate::ndarray::NDDimension::new(8),
                ],
                crate::ndarray::NDDataType::UInt8,
            )
            .unwrap();
        let id = arr.unique_id;
        let to_publish = ad.prepare_array(Arc::new(arr)).unwrap().unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = rt.block_on(ad.array_output.publish(to_publish));

        let received = receiver.blocking_recv().unwrap();
        assert_eq!(received.unique_id, id);
    }
}
