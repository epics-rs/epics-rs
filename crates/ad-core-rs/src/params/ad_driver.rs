use std::sync::Arc;

use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriverBase;

use super::ndarray_driver::NDArrayDriverParams;
use crate::driver::ndarray_driver::ArrayOwner;

/// Additional parameters for ADDriver (detector-specific: gain, shutter, temperature, trigger).
#[derive(Clone, Copy)]
pub struct ADDriverParams {
    pub base: NDArrayDriverParams,
    /// C `FIRST_AD_PARAM` (ADDriver.h:148): every index below it is an
    /// `asynNDArrayDriver` parameter.
    pub first_ad_param: usize,

    // Image size
    pub max_size_x: usize,
    pub max_size_y: usize,
    pub size_x: usize,
    pub size_y: usize,
    pub min_x: usize,
    pub min_y: usize,
    pub bin_x: usize,
    pub bin_y: usize,
    pub reverse_x: usize,
    pub reverse_y: usize,

    // Acquire
    pub image_mode: usize,
    pub num_images: usize,
    pub num_images_counter: usize,
    pub num_exposures: usize,
    pub num_exposures_counter: usize,
    pub acquire_time: usize,
    pub acquire_period: usize,
    pub time_remaining: usize,
    pub status: usize,
    pub status_message: usize,
    pub string_to_server: usize,
    pub string_from_server: usize,
    pub acquire: usize,
    pub acquire_busy: usize,
    pub wait_for_plugins: usize,
    pub read_status: usize,

    // Detector
    pub gain: usize,
    pub frame_type: usize,
    pub trigger_mode: usize,

    // Shutter
    pub shutter_control: usize,
    pub shutter_control_epics: usize,
    pub shutter_status: usize,
    pub shutter_mode: usize,
    pub shutter_open_delay: usize,
    pub shutter_close_delay: usize,

    // Temperature
    pub temperature: usize,
    pub temperature_actual: usize,
}

impl ADDriverParams {
    /// [`NDArrayDriverParams::create`] plus the `ADDriver` parameters.
    pub fn create(port_base: &mut PortDriverBase) -> AsynResult<(Self, Arc<ArrayOwner>)> {
        let (base, owner) = NDArrayDriverParams::create(port_base)?;
        let first_ad_param = port_base.params.len();
        // asynNDArrayDriver creates these (asynNDArrayDriver.cpp:897-899);
        // ADDriver.cpp does not create them again.
        let (acquire, acquire_busy, wait_for_plugins) =
            (base.acquire, base.acquire_busy, base.wait_for_plugins);

        let params = Self {
            base,
            first_ad_param,

            // C createParam order (ADDriver.cpp:139-172), which asynReport
            // prints as each parameter's index.
            gain: port_base.create_param("GAIN", ParamType::Float64)?,
            bin_x: port_base.create_param("BIN_X", ParamType::Int32)?,
            bin_y: port_base.create_param("BIN_Y", ParamType::Int32)?,
            min_x: port_base.create_param("MIN_X", ParamType::Int32)?,
            min_y: port_base.create_param("MIN_Y", ParamType::Int32)?,
            size_x: port_base.create_param("SIZE_X", ParamType::Int32)?,
            size_y: port_base.create_param("SIZE_Y", ParamType::Int32)?,
            max_size_x: port_base.create_param("MAX_SIZE_X", ParamType::Int32)?,
            max_size_y: port_base.create_param("MAX_SIZE_Y", ParamType::Int32)?,
            reverse_x: port_base.create_param("REVERSE_X", ParamType::Int32)?,
            reverse_y: port_base.create_param("REVERSE_Y", ParamType::Int32)?,
            frame_type: port_base.create_param("FRAME_TYPE", ParamType::Int32)?,
            image_mode: port_base.create_param("IMAGE_MODE", ParamType::Int32)?,
            num_exposures: port_base.create_param("NEXPOSURES", ParamType::Int32)?,
            num_exposures_counter: port_base
                .create_param("NEXPOSURES_COUNTER", ParamType::Int32)?,
            num_images: port_base.create_param("NIMAGES", ParamType::Int32)?,
            num_images_counter: port_base.create_param("NIMAGES_COUNTER", ParamType::Int32)?,
            acquire_time: port_base.create_param("ACQ_TIME", ParamType::Float64)?,
            acquire_period: port_base.create_param("ACQ_PERIOD", ParamType::Float64)?,
            time_remaining: port_base.create_param("TIME_REMAINING", ParamType::Float64)?,
            status: port_base.create_param("STATUS", ParamType::Int32)?,
            trigger_mode: port_base.create_param("TRIGGER_MODE", ParamType::Int32)?,
            shutter_control: port_base.create_param("SHUTTER_CONTROL", ParamType::Int32)?,
            shutter_control_epics: port_base
                .create_param("SHUTTER_CONTROL_EPICS", ParamType::Int32)?,
            shutter_status: port_base.create_param("SHUTTER_STATUS", ParamType::Int32)?,
            shutter_mode: port_base.create_param("SHUTTER_MODE", ParamType::Int32)?,
            shutter_open_delay: port_base.create_param("SHUTTER_OPEN_DELAY", ParamType::Float64)?,
            shutter_close_delay: port_base
                .create_param("SHUTTER_CLOSE_DELAY", ParamType::Float64)?,
            temperature: port_base.create_param("TEMPERATURE", ParamType::Float64)?,
            temperature_actual: port_base.create_param("TEMPERATURE_ACTUAL", ParamType::Float64)?,
            read_status: port_base.create_param("READ_STATUS", ParamType::Int32)?,
            status_message: port_base.create_param("STATUS_MESSAGE", ParamType::Octet)?,
            string_to_server: port_base.create_param("STRING_TO_SERVER", ParamType::Octet)?,
            string_from_server: port_base.create_param("STRING_FROM_SERVER", ParamType::Octet)?,
            acquire,
            acquire_busy,
            wait_for_plugins,
        };
        Ok((params, owner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asyn_rs::port::InterfaceMask;
    use asyn_rs::port::PortFlags;

    /// Each parameter is created once, where C creates it: a second
    /// `createParam` of a name is refused (asynPortDriver.cpp:999-1003).
    #[test]
    fn ad_driver_params_create_each_name_once() {
        let mut base = PortDriverBase::new(
            "adonce",
            1,
            PortFlags::default(),
            InterfaceMask::NONE,
            InterfaceMask::NONE,
        );
        let (params, _) = ADDriverParams::create(&mut base).unwrap();
        assert_eq!(params.acquire, params.base.acquire);
        assert_eq!(params.wait_for_plugins, params.base.wait_for_plugins);
        assert_eq!(base.params.len(), params.first_ad_param + 34);
    }

    #[test]
    fn test_create_ad_driver_params() {
        let mut base = PortDriverBase::new(
            "test",
            1,
            PortFlags::default(),
            InterfaceMask::NONE,
            InterfaceMask::NONE,
        );
        let (params, _) = ADDriverParams::create(&mut base).unwrap();
        assert!(base.find_param("ACQUIRE").is_some());
        assert!(base.find_param("GAIN").is_some());
        assert!(base.find_param("SHUTTER_CONTROL").is_some());
        assert!(base.find_param("TEMPERATURE").is_some());
        assert!(base.find_param("SIZE_X").is_some());
        assert_eq!(params.acquire, base.find_param("ACQUIRE").unwrap());
    }

    #[test]
    fn test_ad_driver_param_count() {
        let mut base = PortDriverBase::new(
            "test",
            1,
            PortFlags::default(),
            InterfaceMask::NONE,
            InterfaceMask::NONE,
        );
        let _ = ADDriverParams::create(&mut base).unwrap();
        // base params (~50) + ad-specific (~38) = ~88
        assert!(base.params.len() >= 80);
    }

    #[test]
    fn test_ad_driver_inherits_base() {
        let mut base = PortDriverBase::new(
            "test",
            1,
            PortFlags::default(),
            InterfaceMask::NONE,
            InterfaceMask::NONE,
        );
        let (params, _) = ADDriverParams::create(&mut base).unwrap();
        // Should be able to access base params through the base field
        assert!(base.find_param("ARRAY_COUNTER").is_some());
        assert_eq!(
            params.base.array_counter,
            base.find_param("ARRAY_COUNTER").unwrap()
        );
    }

    /// C's createParam order (ADDriver.cpp:139-172): the index asynReport
    /// prints for each.
    #[test]
    fn params_are_created_in_c_order() {
        let mut base = PortDriverBase::new(
            "order",
            1,
            PortFlags::default(),
            InterfaceMask::NONE,
            InterfaceMask::NONE,
        );
        let (params, _) = ADDriverParams::create(&mut base).unwrap();
        let c = [
            "GAIN",
            "BIN_X",
            "BIN_Y",
            "MIN_X",
            "MIN_Y",
            "SIZE_X",
            "SIZE_Y",
            "MAX_SIZE_X",
            "MAX_SIZE_Y",
            "REVERSE_X",
            "REVERSE_Y",
            "FRAME_TYPE",
            "IMAGE_MODE",
            "NEXPOSURES",
            "NEXPOSURES_COUNTER",
            "NIMAGES",
            "NIMAGES_COUNTER",
            "ACQ_TIME",
            "ACQ_PERIOD",
            "TIME_REMAINING",
            "STATUS",
            "TRIGGER_MODE",
            "SHUTTER_CONTROL",
            "SHUTTER_CONTROL_EPICS",
            "SHUTTER_STATUS",
            "SHUTTER_MODE",
            "SHUTTER_OPEN_DELAY",
            "SHUTTER_CLOSE_DELAY",
            "TEMPERATURE",
            "TEMPERATURE_ACTUAL",
            "READ_STATUS",
            "STATUS_MESSAGE",
            "STRING_TO_SERVER",
            "STRING_FROM_SERVER",
        ];
        for (i, name) in c.iter().enumerate() {
            assert_eq!(
                base.find_param(name),
                Some(params.first_ad_param + i),
                "{name}"
            );
        }
    }
}
