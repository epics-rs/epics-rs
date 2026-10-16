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
use asyn_rs::user::AsynUser;

use crate::driver::ndarray_driver::{
    ArrayOwner, handle_write_int32, init_read_only_params, trace_write_int32,
};
use crate::ndarray::NDArray;
use crate::ndarray_pool::{DriverArray, DriverPool, PublishedArray};
use crate::params::ad_driver::ADDriverParams;
use crate::plugin::channel::{NDArrayOutput, NDArraySender};

use super::{ADStatus, ShutterMode};

/// Base state for ADDriver (extends NDArrayDriver with detector-specific params).
pub struct ADDriverBase {
    pub port_base: PortDriverBase,
    pub params: ADDriverParams,
    pub pool: DriverPool,
    pub array_output: NDArrayOutput,
    pub owner: Arc<ArrayOwner>,
}

impl ADDriverBase {
    pub fn new(
        port_name: &str,
        max_addr: usize,
        max_size_x: i32,
        max_size_y: i32,
        max_memory: usize,
        asyn_flags: PortFlags,
    ) -> AsynResult<Self> {
        // C takes `maxAddr` and `asynFlags` from the detector (ADDriver.cpp:126-133).
        let mut port_base = PortDriverBase::new(port_name, max_addr, asyn_flags);

        let (params, owner) = ADDriverParams::create(&mut port_base)?;
        let pool = DriverPool::for_driver(max_memory, &owner);

        // C `ADDriver::ADDriver` (ADDriver.cpp:150) chains to
        // `asynNDArrayDriver::asynNDArrayDriver`, so its whole read-only block
        // runs for every AD driver before the detector seeds below.
        init_read_only_params(&mut port_base, &params.base, &pool, port_name)?;

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
            owner,
        })
    }

    /// Connect a downstream channel-based receiver.
    pub fn connect_downstream(&mut self, sender: NDArraySender) {
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
    pub fn prepare_array(&mut self, mut array: DriverArray) -> AsynResult<Option<PublishedArray>> {
        let counter = self
            .port_base
            .get_int32_param(self.params.base.array_counter, 0)?
            + 1;
        self.port_base
            .set_int32_param(self.params.base.array_counter, 0, counter)?;

        // C++ `getAttributes(pArray->pAttributeList)` before the callback.
        self.owner
            .attributes()
            .lock()
            .attach(&self.port_base, &mut array);

        // G5/G6/G7: write all per-array parameters (size, dims, type, color,
        // Bayer, timestamps, codec).
        crate::driver::ndarray_driver::write_array_params(
            &mut self.port_base,
            &self.params.base,
            &array,
        )?;
        let array = array.share();

        self.owner.store_array(0, Some(array.array().clone()));

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
            .get_int32_param(self.params.base.array_callbacks, 0)
            .unwrap_or(0)
            != 0;

        let to_publish = if callbacks_enabled {
            self.port_base.set_generic_pointer_param(
                self.params.base.ndarray_data,
                0,
                array.array().clone() as Arc<dyn std::any::Any + Send + Sync>,
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
    /// driven by the shutter hardware / EPICS records, not assumed. C's is void
    /// and drops every param status, so no failure can reach `writeInt32`.
    pub fn set_shutter(&mut self, open: bool) {
        // C ignores the get*Param statuses (ADDriver.cpp:36-38); ADDriver
        // seeds none of the three, so an unset one reads as 0.
        let mode = ShutterMode::from_i32(
            self.port_base
                .get_int32_param(self.params.shutter_mode, 0)
                .unwrap_or(0),
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
                    .get_float64_param(self.params.shutter_open_delay, 0)
                    .unwrap_or(0.0);
                let close_delay = self
                    .port_base
                    .get_float64_param(self.params.shutter_close_delay, 0)
                    .unwrap_or(0.0);
                let _ = self.port_base.set_int32_param(
                    self.params.shutter_control_epics,
                    0,
                    if open { 1 } else { 0 },
                );
                let _ = self.port_base.call_param_callbacks(0);
                // C++: epicsThreadSleep(shutterOpenDelay - shutterCloseDelay).
                let delay = open_delay - close_delay;
                if delay > 0.0 {
                    std::thread::sleep(epics_libcom_rs::runtime::time::duration_from_secs(delay));
                }
            }
        }
    }

    /// Handle a write to a pool-control Int32 parameter (`POOL_EMPTY_FREELIST`,
    /// `POOL_POLL_STATS`, `POOL_PRE_ALLOC_BUFFERS`), mirroring the pool branch
    /// of C++ `asynNDArrayDriver::writeInt32`.
    ///
    /// Returns `true` when `param_index` was a recognized pool-control
    /// parameter.
    pub fn write_int32_pool(&mut self, param_index: usize, _value: i32) -> AsynResult<bool> {
        let template = self.owner.array(0);
        crate::driver::ndarray_driver::handle_pool_write_int32(
            &mut self.port_base,
            &self.params.base,
            &self.pool,
            param_index,
            template.as_deref(),
        )
    }

    /// C++ `asynNDArrayDriver::writeInt32`, which `ADDriver::writeInt32`
    /// hands every parameter below `FIRST_AD_PARAM`.
    fn ndarray_write_int32(&mut self, user: &AsynUser, value: i32) -> AsynResult<()> {
        let template = self.owner.array(0);
        handle_write_int32(
            &mut self.port_base,
            &self.params.base,
            &self.pool,
            template.as_deref(),
            user,
            value,
        )
    }

    /// C++ `ADDriver::writeOctet`, which is `asynNDArrayDriver::writeOctet`,
    /// for an override to chain to.
    pub fn write_octet(&mut self, user: &AsynUser, data: &[u8]) -> AsynResult<usize> {
        self.port_base.write_octet(user, data)
    }

    /// C++ `asynNDArrayDriver::getAttributes`, for a driver that fills the
    /// array's attribute list itself before publishing.
    pub fn attach_attributes(&mut self, array: &mut NDArray) {
        self.owner
            .attributes()
            .lock()
            .attach(&self.port_base, array);
    }

    /// Store `ADAcquire`; its rule keeps `ADAcquireBusy` in step.
    pub fn set_acquire(&mut self, value: i32) -> AsynResult<()> {
        self.port_base
            .set_int32_param(self.params.acquire, 0, value)
    }

    /// Store `NDNumQueuedArrays`; its rule clears `ADAcquireBusy` once the
    /// queue drains after acquisition stopped.
    pub fn set_num_queued_arrays(&mut self, value: i32) -> AsynResult<()> {
        self.port_base
            .set_int32_param(self.params.base.num_queued_arrays, 0, value)
    }
}

/// Trait for areaDetector drivers.
pub trait ADDriver: asyn_rs::port::PortDriver {
    fn ad_base(&self) -> &ADDriverBase;
    fn ad_base_mut(&mut self) -> &mut ADDriverBase;

    /// C++ virtual `ADDriver::setShutter` (ADDriver.cpp:29-52); a detector
    /// that drives its own shutter overrides it.
    fn set_shutter(&mut self, open: bool) {
        self.ad_base_mut().set_shutter(open)
    }

    /// The `asynNDArrayDriver::writeInt32` call `ad_write_int32` makes
    /// (ADDriver.cpp:101); a driver whose `pArrays[0]` is not in its
    /// `ArrayOwner` overrides it to pass that array to `handle_write_int32`.
    fn ndarray_write_int32(&mut self, user: &AsynUser, value: i32) -> AsynResult<()> {
        self.ad_base_mut().ndarray_write_int32(user, value)
    }

    /// C++ `ADDriver::writeInt32` (ADDriver.cpp:84-116). A driver's
    /// `write_int32` calls it for every parameter it does not own.
    fn ad_write_int32(&mut self, user: &AsynUser, value: i32) -> AsynResult<()> {
        let mut status =
            self.ad_base_mut()
                .port_base
                .set_int32_param(user.reason, user.addr, value);
        let params = &self.ad_base().params;
        if user.reason == params.shutter_control {
            self.set_shutter(value != 0);
        } else if user.reason < params.first_ad_param {
            status = ADDriver::ndarray_write_int32(self, user, value);
        }
        let ad = self.ad_base_mut();
        let _ = ad.port_base.call_param_callbacks(user.addr);
        trace_write_int32(&ad.port_base, user, "ADDriver", &status, value);
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::ndarray_driver::tests::{assert_c_read_only_block, assert_fresh_pool_stats};
    use crate::plugin::runtime::ASYN_CANBLOCK;
    use asyn_rs::param::ParamType;
    use asyn_rs::param::ParamValue;
    use asyn_rs::registry::PortRegistry;
    use asyn_rs::request::ParamSetValue;
    use asyn_rs::runtime::RuntimeConfig;
    use asyn_rs::runtime::port::create_port_runtime;

    #[test]
    fn test_constructor_seeds_the_c_read_only_block() {
        // C++ `ADDriver::ADDriver` (ADDriver.cpp:150) chains to
        // `asynNDArrayDriver::asynNDArrayDriver`, so the whole of
        // asynNDArrayDriver.cpp:954-1005 must be readable on a detector port
        // too, not just the six seeds this constructor used to repeat.
        let ad = ADDriverBase::new("TEST", 1, 1024, 768, 50_000_000, ASYN_CANBLOCK).unwrap();
        assert_c_read_only_block(&ad.port_base, &ad.params.base, "TEST");
        assert_fresh_pool_stats(&ad.port_base, &ad.params.base);
        // Neither constructor sets NDCodec; only a published array does.
        assert!(
            ad.port_base
                .get_string_param(ad.params.base.codec, 0)
                .is_err()
        );
        // ADDriver.cpp sets neither; the concrete driver or the DB does.
        for reason in [ad.params.base.array_callbacks, ad.params.base.data_type] {
            assert!(ad.port_base.get_int32_param(reason, 0).is_err());
        }
    }

    /// A detector that overrides `setShutter`, as marCCD and simDetector do.
    struct OwnShutter {
        ad: ADDriverBase,
        shutter: Vec<bool>,
        nd_writes: Vec<usize>,
    }

    impl asyn_rs::port::PortDriver for OwnShutter {
        fn base(&self) -> &PortDriverBase {
            &self.ad.port_base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.ad.port_base
        }
    }

    impl ADDriver for OwnShutter {
        fn ad_base(&self) -> &ADDriverBase {
            &self.ad
        }
        fn ad_base_mut(&mut self) -> &mut ADDriverBase {
            &mut self.ad
        }
        fn set_shutter(&mut self, open: bool) {
            self.shutter.push(open);
        }
        fn ndarray_write_int32(&mut self, user: &AsynUser, value: i32) -> AsynResult<()> {
            self.nd_writes.push(user.reason);
            self.ad.ndarray_write_int32(user, value)
        }
    }

    /// A detector that keeps C's `ADDriver::setShutter`.
    struct BaseShutter(ADDriverBase);

    impl asyn_rs::port::PortDriver for BaseShutter {
        fn base(&self) -> &PortDriverBase {
            &self.0.port_base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.0.port_base
        }
    }

    impl ADDriver for BaseShutter {
        fn ad_base(&self) -> &ADDriverBase {
            &self.0
        }
        fn ad_base_mut(&mut self) -> &mut ADDriverBase {
            &mut self.0
        }
    }

    fn own_shutter() -> OwnShutter {
        OwnShutter {
            ad: ADDriverBase::new("TEST", 1, 8, 8, 1_000_000, ASYN_CANBLOCK).unwrap(),
            shutter: Vec::new(),
            nd_writes: Vec::new(),
        }
    }

    /// Only a parameter below FIRST_AD_PARAM reaches asynNDArrayDriver
    /// (ADDriver.cpp:97-101), through the driver's override.
    #[test]
    fn a_base_class_write_reaches_an_overridden_ndarray_write_int32() {
        let mut d = own_shutter();
        let p = d.ad.params;
        d.ad_write_int32(&AsynUser::new(p.base.pool_poll_stats), 1)
            .unwrap();
        d.ad_write_int32(&AsynUser::new(p.shutter_control), 1)
            .unwrap();
        d.ad_write_int32(&AsynUser::new(p.image_mode), 1).unwrap();
        assert_eq!(d.nd_writes, vec![p.base.pool_poll_stats]);
    }

    #[test]
    fn first_ad_param_splits_the_ndarray_params_from_the_detector_params() {
        let ad = ADDriverBase::new("TEST", 1, 8, 8, 1_000_000, ASYN_CANBLOCK).unwrap();
        let p = &ad.params;
        assert!(p.base.pool_poll_stats < p.first_ad_param);
        assert!(p.base.array_callbacks < p.first_ad_param);
        assert!(p.max_size_x >= p.first_ad_param);
        assert!(p.gain >= p.first_ad_param);
        assert!(p.shutter_control >= p.first_ad_param);
    }

    #[test]
    fn ad_write_int32_shutter_control_calls_the_drivers_set_shutter() {
        let mut d = own_shutter();
        let reason = d.ad.params.shutter_control;
        d.ad_write_int32(&AsynUser::new(reason), 1).unwrap();
        d.ad_write_int32(&AsynUser::new(reason), 0).unwrap();
        assert_eq!(d.shutter, [true, false]);
        assert_eq!(d.ad.port_base.get_int32_param(reason, 0).unwrap(), 0);
    }

    #[test]
    fn ad_write_int32_shutter_control_defaults_to_add_driver_set_shutter() {
        let mut d =
            BaseShutter(ADDriverBase::new("TEST", 1, 8, 8, 1_000_000, ASYN_CANBLOCK).unwrap());
        let p = d.0.params;
        d.0.port_base
            .set_int32_param(p.shutter_mode, 0, ShutterMode::EpicsOnly as i32)
            .unwrap();
        d.ad_write_int32(&AsynUser::new(p.shutter_control), 1)
            .unwrap();
        assert_eq!(
            d.0.port_base
                .get_int32_param(p.shutter_control_epics, 0)
                .unwrap(),
            1
        );
    }

    #[test]
    fn ad_write_int32_hands_an_ndarray_param_to_the_pool_branch() {
        let mut d = own_shutter();
        let p = d.ad.params.base;
        let arr =
            d.ad.pool
                .alloc(
                    vec![crate::ndarray::NDDimension::new(64)],
                    crate::ndarray::NDDataType::UInt8,
                )
                .unwrap();
        assert_eq!(
            d.ad.port_base
                .get_int32_param(p.pool_alloc_buffers, 0)
                .unwrap(),
            0
        );
        d.ad_write_int32(&AsynUser::new(p.pool_poll_stats), 1)
            .unwrap();
        assert_eq!(
            d.ad.port_base
                .get_int32_param(p.pool_alloc_buffers, 0)
                .unwrap(),
            1
        );
        d.ad.pool.release(arr);

        // No array yet to copy: C prints the refusal and still clears the
        // command (asynNDArrayDriver.cpp:686-688, :715-721).
        d.ad_write_int32(&AsynUser::new(p.pool_pre_alloc), 1)
            .unwrap();
        assert_eq!(
            d.ad.port_base.get_int32_param(p.pool_pre_alloc, 0).unwrap(),
            0
        );
        assert!(d.shutter.is_empty());
    }

    #[test]
    fn ad_write_int32_stores_a_detector_param_without_the_pool_branch() {
        let mut d = own_shutter();
        let own =
            d.ad.port_base
                .create_param("OWN", ParamType::Int32)
                .unwrap();
        d.ad_write_int32(&AsynUser::new(d.ad.params.image_mode), 2)
            .unwrap();
        d.ad_write_int32(&AsynUser::new(own), 7).unwrap();
        assert_eq!(
            d.ad.port_base
                .get_int32_param(d.ad.params.image_mode, 0)
                .unwrap(),
            2
        );
        assert_eq!(d.ad.port_base.get_int32_param(own, 0).unwrap(), 7);
        assert!(d.shutter.is_empty());
    }

    #[test]
    fn ad_write_int32_returns_the_store_status() {
        let mut d = own_shutter();
        assert!(
            d.ad_write_int32(&AsynUser::new(usize::MAX >> 1), 1)
                .is_err()
        );
    }

    #[test]
    fn max_addr_comes_from_the_caller() {
        // eiger passes MAX_ASYN_ADDRESS, mythen is multi-device: ADDriver.cpp:126.
        let ad = ADDriverBase::new("TEST", 4, 8, 8, 1_000_000, ASYN_CANBLOCK).unwrap();
        assert_eq!(ad.port_base.max_addr, 4);
    }

    #[test]
    fn test_new_sets_initial_params() {
        let ad = ADDriverBase::new("TEST", 1, 1024, 768, 50_000_000, ASYN_CANBLOCK).unwrap();
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
        let ad = ADDriverBase::new("TEST", 1, 1024, 768, 50_000_000, ASYN_CANBLOCK).unwrap();

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
            assert!(
                matches!(
                    ad.port_base.get_int32_param(idx, 0),
                    Err(asyn_rs::error::AsynError::ParamUndefined(_))
                ),
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
            assert!(
                matches!(
                    ad.port_base.get_float64_param(idx, 0),
                    Err(asyn_rs::error::AsynError::ParamUndefined(_))
                ),
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

    /// An `NDAttributesFile` write loads the set, and every prepared array
    /// carries it with `PARAM` values read at that moment — C++
    /// `asynNDArrayDriver::writeOctet` and `getAttributes`, which `ADDriver`
    /// inherits.
    #[test]
    fn test_attributes_file_write_reaches_the_prepared_array() {
        use crate::attributes::NDAttrValue;
        let mut ad = ADDriverBase::new("TEST", 1, 8, 8, 1_000_000, ASYN_CANBLOCK).unwrap();
        // The ArrayCallbacks record's PINI; the driver leaves it undefined.
        ad.port_base
            .set_int32_param(ad.params.base.array_callbacks, 0, 1)
            .unwrap();
        let xml = r#"<Attributes>
            <Attribute name="Gain" type="PARAM" source="GAIN" datatype="DOUBLE"/>
        </Attributes>"#;
        let file = ad.params.base.attributes_file;
        ad.write_octet(&AsynUser::new(file), xml.as_bytes())
            .unwrap();

        for gain in [2.5, 4.0] {
            ad.port_base
                .set_float64_param(ad.params.gain, 0, gain)
                .unwrap();
            let arr = ad
                .pool
                .alloc(
                    vec![crate::ndarray::NDDimension::new(8)],
                    crate::ndarray::NDDataType::UInt8,
                )
                .unwrap();
            let out = ad.prepare_array(arr).unwrap().unwrap();
            assert_eq!(
                out.attributes.get("Gain").unwrap().value,
                NDAttrValue::Float64(gain)
            );
        }
    }

    #[test]
    fn test_prepare_array_increments_counter() {
        let mut ad = ADDriverBase::new("TEST", 1, 256, 256, 50_000_000, ASYN_CANBLOCK).unwrap();
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
        ad.prepare_array(arr).unwrap();
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

        let mut ad = ADDriverBase::new("TEST", 1, 64, 64, 1_000_000, ASYN_CANBLOCK).unwrap();
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
        ad.prepare_array(arr).unwrap();

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
        let mut ad = ADDriverBase::new("TEST", 1, 8, 8, 1_000_000, ASYN_CANBLOCK).unwrap();
        // The ArrayCallbacks record's PINI; the driver leaves it undefined.
        ad.port_base
            .set_int32_param(ad.params.base.array_callbacks, 0, 1)
            .unwrap();
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
        ad.prepare_array(arr).unwrap();

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
        let mut ad = ADDriverBase::new("TEST", 1, 8, 8, 1_000_000, ASYN_CANBLOCK).unwrap();
        ad.port_base
            .set_int32_param(ad.params.shutter_mode, 0, ShutterMode::DetectorOnly as i32)
            .unwrap();
        ad.port_base
            .set_int32_param(ad.params.shutter_control, 0, 7)
            .unwrap();

        ad.set_shutter(true);
        // Unchanged — base setShutter does nothing for detector mode.
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.shutter_control, 0)
                .unwrap(),
            7
        );
    }

    #[test]
    fn set_shutter_with_shutter_mode_unset_reads_it_as_none() {
        let mut ad = ADDriverBase::new("TEST", 1, 8, 8, 1_000_000, ASYN_CANBLOCK).unwrap();
        ad.set_shutter(true);
        assert!(
            ad.port_base
                .get_int32_param(ad.params.shutter_control_epics, 0)
                .is_err(),
            "mode 0 (None) leaves ShutterControlEPICS unwritten"
        );
    }

    #[test]
    fn test_shutter_control_epics_mode() {
        let mut ad = ADDriverBase::new("TEST", 1, 8, 8, 1_000_000, ASYN_CANBLOCK).unwrap();
        ad.port_base
            .set_int32_param(ad.params.shutter_mode, 0, ShutterMode::EpicsOnly as i32)
            .unwrap();

        ad.set_shutter(true);
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
                .get_int32_param(ad.params.shutter_status, 0)
                .ok(),
            Some(0),
            "SHUTTER_STATUS must keep its constructor value through setShutter"
        );
    }

    #[test]
    fn test_shutter_epics_mode_sleeps_open_minus_close() {
        // C parity: EPICS mode sleeps (shutterOpenDelay - shutterCloseDelay).
        let mut ad = ADDriverBase::new("TEST", 1, 8, 8, 1_000_000, ASYN_CANBLOCK).unwrap();
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
        ad.set_shutter(true);
        let elapsed = start.elapsed();
        // delay = 0.05 - 0.01 = 0.04 s.
        assert!(
            elapsed >= std::time::Duration::from_millis(35),
            "expected ~40ms sleep, got {elapsed:?}"
        );
    }

    fn busy(ad: &ADDriverBase) -> i32 {
        ad.port_base
            .get_int32_param(ad.params.acquire_busy, 0)
            .unwrap()
    }

    #[test]
    fn every_acquire_store_drives_acquire_busy() {
        let mut ad = ADDriverBase::new("TEST", 1, 8, 8, 1_000_000, ASYN_CANBLOCK).unwrap();
        let acquire = ad.params.acquire;
        ad.port_base.set_int32_param(acquire, 0, 1).unwrap();
        assert_eq!(busy(&ad), 1);
        // A driver's background ParamSetValue stores through the same rule.
        ad.port_base
            .set_param_value(acquire, 0, ParamValue::Int32(0))
            .unwrap();
        assert_eq!(busy(&ad), 0);
    }

    #[test]
    fn a_param_set_value_through_the_port_clears_acquire_busy() {
        struct Det(ADDriverBase);
        impl asyn_rs::port::PortDriver for Det {
            fn base(&self) -> &PortDriverBase {
                &self.0.port_base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0.port_base
            }
        }
        let ad = ADDriverBase::new("ADRULEPORT", 1, 8, 8, 1_000_000, ASYN_CANBLOCK).unwrap();
        let (acquire, acquire_busy) = (ad.params.acquire, ad.params.acquire_busy);
        let claim = PortRegistry::new().claim("ADRULEPORT").unwrap();
        let (runtime, _actor) =
            create_port_runtime(&claim, Det(ad), RuntimeConfig::default()).unwrap();
        let handle = runtime.port_handle().clone();
        let read_busy = move |d: &mut Det| d.0.port_base.get_int32_param(acquire_busy, 0).unwrap();
        for (value, want) in [(1, 1), (0, 0)] {
            handle
                .set_params_and_notify_blocking(
                    0,
                    vec![ParamSetValue::new(acquire, 0, ParamValue::Int32(value))],
                )
                .unwrap();
            assert_eq!(handle.with_driver_blocking(read_busy).unwrap(), want);
        }
    }

    #[test]
    fn wait_for_plugins_holds_acquire_busy_until_the_queue_drains_after_stop() {
        let mut ad = ADDriverBase::new("TEST", 1, 8, 8, 1_000_000, ASYN_CANBLOCK).unwrap();
        let (acquire, queued) = (ad.params.acquire, ad.params.base.num_queued_arrays);
        ad.port_base
            .set_int32_param(ad.params.wait_for_plugins, 0, 1)
            .unwrap();
        ad.port_base.set_int32_param(acquire, 0, 1).unwrap();
        ad.owner.queued().increment();

        // The queue draining while still acquiring leaves it busy.
        ad.port_base.set_int32_param(queued, 0, 0).unwrap();
        assert_eq!(busy(&ad), 1);
        // A stop with an array queued keeps it busy (the live count, not the
        // NDNumQueuedArrays parameter, which already reads 0).
        ad.port_base.set_int32_param(acquire, 0, 0).unwrap();
        assert_eq!(busy(&ad), 1);
        // A non-zero queue store clears nothing.
        ad.port_base.set_int32_param(queued, 0, 1).unwrap();
        assert_eq!(busy(&ad), 1);
        ad.owner.queued().decrement();
        ad.port_base.set_int32_param(queued, 0, 0).unwrap();
        assert_eq!(busy(&ad), 0);
    }

    #[test]
    fn a_stop_with_an_empty_queue_clears_acquire_busy_under_wait_for_plugins() {
        let mut ad = ADDriverBase::new("TEST", 1, 8, 8, 1_000_000, ASYN_CANBLOCK).unwrap();
        ad.port_base
            .set_int32_param(ad.params.wait_for_plugins, 0, 1)
            .unwrap();
        ad.port_base
            .set_int32_param(ad.params.acquire, 0, 1)
            .unwrap();
        ad.port_base
            .set_int32_param(ad.params.acquire, 0, 0)
            .unwrap();
        assert_eq!(busy(&ad), 0);
    }

    #[test]
    fn test_set_acquire_drives_acquire_busy() {
        // G8: ADAcquire=1 sets AcquireBusy=1; ADAcquire=0 (no WaitForPlugins)
        // immediately clears AcquireBusy.
        let mut ad = ADDriverBase::new("TEST", 1, 8, 8, 1_000_000, ASYN_CANBLOCK).unwrap();
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
        let mut ad = ADDriverBase::new("TEST", 1, 8, 8, 1_000_000, ASYN_CANBLOCK).unwrap();
        ad.port_base
            .set_int32_param(ad.params.wait_for_plugins, 0, 1)
            .unwrap();
        ad.set_acquire(1).unwrap();
        ad.owner.queued().increment(); // one array still queued

        ad.set_acquire(0).unwrap();
        // Still busy — queue not drained.
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.acquire_busy, 0)
                .unwrap(),
            1
        );

        // Drain the queue, then NDNumQueuedArrays=0 clears AcquireBusy.
        ad.owner.queued().decrement();
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
        let mut ad = ADDriverBase::new("TEST", 1, 8, 8, 10_000_000, ASYN_CANBLOCK).unwrap();
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
        let mut ad = ADDriverBase::new("TEST", 1, 8, 8, 10_000_000, ASYN_CANBLOCK).unwrap();
        let arr = ad
            .pool
            .alloc(
                vec![crate::ndarray::NDDimension::new(64)],
                crate::ndarray::NDDataType::UInt8,
            )
            .unwrap();
        ad.prepare_array(arr).unwrap();
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

    /// A task that publishes without `prepare_array` reaches the driver's
    /// `pArrays[0]` through its pool, as C's tasks write `this->pArrays[0]`.
    #[test]
    fn an_array_stored_through_the_pool_is_the_pre_alloc_template() {
        let mut ad = ADDriverBase::new("TEST", 1, 8, 8, 10_000_000, ASYN_CANBLOCK).unwrap();
        let task_pool = ad.pool.clone();
        let arr = task_pool
            .alloc(
                vec![crate::ndarray::NDDimension::new(64)],
                crate::ndarray::NDDataType::UInt8,
            )
            .unwrap();
        task_pool
            .owner()
            .store_array(0, Some(arr.share().array().clone()));
        assert_eq!(ad.pool.allocated_bytes(), 64, "the slot keeps it charged");
        ad.port_base
            .set_int32_param(ad.params.base.pool_num_pre_alloc_buffers, 0, 2)
            .unwrap();
        ad.write_int32_pool(ad.params.base.pool_pre_alloc, 1)
            .unwrap();
        assert_eq!(ad.pool.num_free_buffers(), 2);

        // Released before a refused alloc, the slot leaves no template.
        task_pool.owner().store_array(0, None);
        ad.write_int32_pool(ad.params.base.pool_empty_free_list, 1)
            .unwrap();
        ad.write_int32_pool(ad.params.base.pool_pre_alloc, 1)
            .unwrap();
        assert_eq!(ad.pool.num_free_buffers(), 0);
    }

    #[test]
    fn test_write_int32_pool_unrecognized() {
        // G3: a non-pool parameter is not handled.
        let mut ad = ADDriverBase::new("TEST", 1, 8, 8, 1_000_000, ASYN_CANBLOCK).unwrap();
        let handled = ad.write_int32_pool(ad.params.gain, 1).unwrap();
        assert!(!handled);
    }

    #[test]
    fn test_gain_and_temperature() {
        // The constructor leaves both to the database (ADDriver.cpp:174-179);
        // once written, they read back through the parameter library.
        let mut ad = ADDriverBase::new("TEST", 1, 8, 8, 1_000_000, ASYN_CANBLOCK).unwrap();
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

        let mut ad = ADDriverBase::new("TEST", 1, 8, 8, 1_000_000, ASYN_CANBLOCK).unwrap();
        // The ArrayCallbacks record's PINI; the driver leaves it undefined.
        ad.port_base
            .set_int32_param(ad.params.base.array_callbacks, 0, 1)
            .unwrap();
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
        let to_publish = ad.prepare_array(arr).unwrap().unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = rt.block_on(ad.array_output.publish(to_publish));

        let received = receiver.blocking_recv().unwrap();
        assert_eq!(received.unique_id, id);
    }
}
