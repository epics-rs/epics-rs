use std::sync::Arc;

use asyn_rs::error::AsynResult;
use asyn_rs::port::{PortDriverBase, PortFlags};

use crate::color::NDColorMode;
use crate::ndarray::NDArray;
use crate::ndarray_pool::NDArrayPool;
use crate::params::ad_driver::ADDriverParams;
use crate::plugin::channel::{NDArrayOutput, NDArraySender, QueuedArrayCounter};

use super::{ADStatus, ImageMode, ShutterMode};

/// Base state for ADDriver (extends NDArrayDriver with detector-specific params).
pub struct ADDriverBase {
    pub port_base: PortDriverBase,
    pub params: ADDriverParams,
    pub pool: Arc<NDArrayPool>,
    pub array_output: NDArrayOutput,
    pub queued_counter: Arc<QueuedArrayCounter>,
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

        // Set initial values
        // Identity strings
        port_base.set_string_param(params.base.port_name_self, 0, port_name.into())?;
        port_base.set_string_param(
            params.base.ad_core_version,
            0,
            env!("CARGO_PKG_VERSION").into(),
        )?;
        port_base.set_string_param(params.base.driver_version, 0, "0.0.0".into())?;
        port_base.set_string_param(params.base.codec, 0, String::new())?;

        port_base.set_int32_param(params.max_size_x, 0, max_size_x)?;
        port_base.set_int32_param(params.max_size_y, 0, max_size_y)?;
        port_base.set_int32_param(params.size_x, 0, max_size_x)?;
        port_base.set_int32_param(params.size_y, 0, max_size_y)?;
        port_base.set_int32_param(params.bin_x, 0, 1)?;
        port_base.set_int32_param(params.bin_y, 0, 1)?;
        port_base.set_int32_param(params.image_mode, 0, ImageMode::Single as i32)?;
        port_base.set_int32_param(params.num_images, 0, 1)?;
        port_base.set_int32_param(params.num_exposures, 0, 1)?;
        port_base.set_float64_param(params.acquire_time, 0, 1.0)?;
        port_base.set_float64_param(params.acquire_period, 0, 1.0)?;
        port_base.set_int32_param(params.status, 0, ADStatus::Idle as i32)?;
        port_base.set_string_param(params.status_message, 0, "Idle".into())?;
        port_base.set_int32_param(params.base.data_type, 0, 1)?; // UInt8
        port_base.set_int32_param(params.base.color_mode, 0, NDColorMode::Mono as i32)?;
        port_base.set_int32_param(params.base.array_callbacks, 0, 1)?;
        port_base.set_float64_param(
            params.base.pool_max_memory,
            0,
            max_memory as f64 / 1_048_576.0,
        )?;
        // Initial array size based on detector dimensions and data type (UInt8)
        port_base.set_int32_param(params.base.array_size_x, 0, max_size_x)?;
        port_base.set_int32_param(params.base.array_size_y, 0, max_size_y)?;
        port_base.set_int32_param(params.base.array_size_z, 0, 0)?;
        let initial_array_bytes = max_size_x as i64 * max_size_y as i64; // UInt8 = 1 byte/element
        port_base.set_int32_param(params.base.array_size, 0, initial_array_bytes as i32)?;

        port_base.set_float64_param(params.gain, 0, 1.0)?;
        port_base.set_int32_param(params.shutter_mode, 0, ShutterMode::None as i32)?;
        port_base.set_float64_param(params.temperature, 0, 25.0)?;
        port_base.set_float64_param(params.temperature_actual, 0, 25.0)?;

        let pool = Arc::new(NDArrayPool::new(max_memory));

        Ok(Self {
            port_base,
            params,
            pool,
            array_output: NDArrayOutput::new(),
            queued_counter: Arc::new(QueuedArrayCounter::new()),
        })
    }

    /// Connect a downstream channel-based receiver.
    pub fn connect_downstream(&mut self, mut sender: NDArraySender) {
        sender.set_queued_counter(self.queued_counter.clone());
        self.array_output.add(sender);
    }

    /// Publish an array: update counters, push to plugins and channel outputs, fire callbacks.
    pub fn publish_array(&mut self, array: Arc<NDArray>) -> AsynResult<()> {
        let counter = self
            .port_base
            .get_int32_param(self.params.base.array_counter, 0)?
            + 1;
        self.port_base
            .set_int32_param(self.params.base.array_counter, 0, counter)?;

        let info = array.info();
        self.port_base
            .set_int32_param(self.params.base.array_size_x, 0, info.x_size as i32)?;
        self.port_base
            .set_int32_param(self.params.base.array_size_y, 0, info.y_size as i32)?;
        self.port_base
            .set_int32_param(self.params.base.array_size_z, 0, info.color_size as i32)?;
        self.port_base
            .set_int32_param(self.params.base.array_size, 0, info.total_bytes as i32)?;

        self.port_base.set_float64_param(
            self.params.base.pool_used_memory,
            0,
            self.pool.allocated_bytes() as f64 / 1_048_576.0,
        )?;

        let callbacks_enabled = self
            .port_base
            .get_int32_param(self.params.base.array_callbacks, 0)?
            != 0;

        if callbacks_enabled {
            self.port_base.set_generic_pointer_param(
                self.params.base.ndarray_data,
                0,
                array.clone() as Arc<dyn std::any::Any + Send + Sync>,
            )?;

            self.array_output.publish(array);
        }

        self.port_base.call_param_callbacks(0)?;

        Ok(())
    }

    /// Set shutter state (open/close). In C++ this dispatches based on shutter mode.
    pub fn set_shutter(&mut self, open: bool) -> AsynResult<()> {
        let mode = ShutterMode::from_i32(
            self.port_base
                .get_int32_param(self.params.shutter_mode, 0)?,
        );

        match mode {
            ShutterMode::None => {}
            ShutterMode::DetectorOnly | ShutterMode::EpicsAndDetector => {
                self.port_base.set_int32_param(
                    self.params.shutter_control,
                    0,
                    if open { 1 } else { 0 },
                )?;
            }
            ShutterMode::EpicsOnly => {
                self.port_base.set_int32_param(
                    self.params.shutter_control_epics,
                    0,
                    if open { 1 } else { 0 },
                )?;
            }
        }

        self.port_base
            .set_int32_param(self.params.shutter_status, 0, if open { 1 } else { 0 })?;

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

    #[test]
    fn test_new_sets_initial_params() {
        let ad = ADDriverBase::new("TEST", 1024, 768, 50_000_000).unwrap();
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
            ad.port_base.get_int32_param(ad.params.size_x, 0).unwrap(),
            1024
        );
        assert_eq!(
            ad.port_base.get_int32_param(ad.params.size_y, 0).unwrap(),
            768
        );
        assert_eq!(
            ad.port_base.get_int32_param(ad.params.status, 0).unwrap(),
            ADStatus::Idle as i32
        );
    }

    #[test]
    fn test_publish_array_increments_counter() {
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
        ad.publish_array(Arc::new(arr)).unwrap();
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.base.array_counter, 0)
                .unwrap(),
            1
        );
    }

    #[test]
    fn test_publish_array_skips_output_when_callbacks_disabled() {
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
        ad.publish_array(Arc::new(arr)).unwrap();

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
        ad.publish_array(Arc::new(arr)).unwrap();

        let gp = ad
            .port_base
            .get_generic_pointer_param(ad.params.base.ndarray_data, 0)
            .unwrap();
        let recovered = gp.downcast_ref::<NDArray>().unwrap();
        assert_eq!(recovered.unique_id, id);
    }

    #[test]
    fn test_shutter_control_detector_mode() {
        let mut ad = ADDriverBase::new("TEST", 8, 8, 1_000_000).unwrap();
        ad.port_base
            .set_int32_param(ad.params.shutter_mode, 0, ShutterMode::DetectorOnly as i32)
            .unwrap();

        ad.set_shutter(true).unwrap();
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.shutter_control, 0)
                .unwrap(),
            1
        );
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.shutter_status, 0)
                .unwrap(),
            1
        );

        ad.set_shutter(false).unwrap();
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.shutter_control, 0)
                .unwrap(),
            0
        );
        assert_eq!(
            ad.port_base
                .get_int32_param(ad.params.shutter_status, 0)
                .unwrap(),
            0
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
    }

    #[test]
    fn test_gain_and_temperature() {
        let ad = ADDriverBase::new("TEST", 8, 8, 1_000_000).unwrap();
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
        ad.publish_array(Arc::new(arr)).unwrap();

        let received = receiver.blocking_recv().unwrap();
        assert_eq!(received.unique_id, id);
    }
}
