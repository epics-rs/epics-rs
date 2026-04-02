use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriverBase;

use super::ndarray_driver::NDArrayDriverParams;

/// Additional parameters for ADDriver (detector-specific: gain, shutter, temperature, trigger).
#[derive(Clone, Copy)]
pub struct ADDriverParams {
    pub base: NDArrayDriverParams,

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
    pub shutter_status_epics: usize,
    pub shutter_mode: usize,
    pub shutter_open_delay: usize,
    pub shutter_close_delay: usize,

    // Temperature
    pub temperature: usize,
    pub temperature_actual: usize,
}

impl ADDriverParams {
    pub fn create(port_base: &mut PortDriverBase) -> AsynResult<Self> {
        let base = NDArrayDriverParams::create(port_base)?;

        Ok(Self {
            base,

            // Image size
            max_size_x: port_base.create_param("MAX_SIZE_X", ParamType::Int32)?,
            max_size_y: port_base.create_param("MAX_SIZE_Y", ParamType::Int32)?,
            size_x: port_base.create_param("SIZE_X", ParamType::Int32)?,
            size_y: port_base.create_param("SIZE_Y", ParamType::Int32)?,
            min_x: port_base.create_param("MIN_X", ParamType::Int32)?,
            min_y: port_base.create_param("MIN_Y", ParamType::Int32)?,
            bin_x: port_base.create_param("BIN_X", ParamType::Int32)?,
            bin_y: port_base.create_param("BIN_Y", ParamType::Int32)?,
            reverse_x: port_base.create_param("REVERSE_X", ParamType::Int32)?,
            reverse_y: port_base.create_param("REVERSE_Y", ParamType::Int32)?,

            // Acquire
            image_mode: port_base.create_param("IMAGE_MODE", ParamType::Int32)?,
            num_images: port_base.create_param("NIMAGES", ParamType::Int32)?,
            num_images_counter: port_base.create_param("NIMAGES_COUNTER", ParamType::Int32)?,
            num_exposures: port_base.create_param("NEXPOSURES", ParamType::Int32)?,
            num_exposures_counter: port_base
                .create_param("NEXPOSURES_COUNTER", ParamType::Int32)?,
            acquire_time: port_base.create_param("ACQ_TIME", ParamType::Float64)?,
            acquire_period: port_base.create_param("ACQ_PERIOD", ParamType::Float64)?,
            time_remaining: port_base.create_param("TIME_REMAINING", ParamType::Float64)?,
            status: port_base.create_param("STATUS", ParamType::Int32)?,
            status_message: port_base.create_param("STATUS_MESSAGE", ParamType::Octet)?,
            string_to_server: port_base.create_param("STRING_TO_SERVER", ParamType::Octet)?,
            string_from_server: port_base.create_param("STRING_FROM_SERVER", ParamType::Octet)?,
            acquire: port_base.create_param("ACQUIRE", ParamType::Int32)?,
            acquire_busy: port_base.create_param("ACQUIRE_BUSY", ParamType::Int32)?,
            wait_for_plugins: port_base.create_param("WAIT_FOR_PLUGINS", ParamType::Int32)?,
            read_status: port_base.create_param("READ_STATUS", ParamType::Int32)?,

            // Detector
            gain: port_base.create_param("GAIN", ParamType::Float64)?,
            frame_type: port_base.create_param("FRAME_TYPE", ParamType::Int32)?,
            trigger_mode: port_base.create_param("TRIGGER_MODE", ParamType::Int32)?,

            // Shutter
            shutter_control: port_base.create_param("SHUTTER_CONTROL", ParamType::Int32)?,
            shutter_control_epics: port_base
                .create_param("SHUTTER_CONTROL_EPICS", ParamType::Int32)?,
            shutter_status: port_base.create_param("SHUTTER_STATUS", ParamType::Int32)?,
            shutter_status_epics: port_base
                .create_param("SHUTTER_STATUS_EPICS", ParamType::Int32)?,
            shutter_mode: port_base.create_param("SHUTTER_MODE", ParamType::Int32)?,
            shutter_open_delay: port_base.create_param("SHUTTER_OPEN_DELAY", ParamType::Float64)?,
            shutter_close_delay: port_base
                .create_param("SHUTTER_CLOSE_DELAY", ParamType::Float64)?,

            // Temperature
            temperature: port_base.create_param("TEMPERATURE", ParamType::Float64)?,
            temperature_actual: port_base.create_param("TEMPERATURE_ACTUAL", ParamType::Float64)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asyn_rs::port::PortFlags;

    #[test]
    fn test_create_ad_driver_params() {
        let mut base = PortDriverBase::new("test", 1, PortFlags::default());
        let params = ADDriverParams::create(&mut base).unwrap();
        assert!(base.find_param("ACQUIRE").is_some());
        assert!(base.find_param("GAIN").is_some());
        assert!(base.find_param("SHUTTER_CONTROL").is_some());
        assert!(base.find_param("TEMPERATURE").is_some());
        assert!(base.find_param("SIZE_X").is_some());
        assert_eq!(params.acquire, base.find_param("ACQUIRE").unwrap());
    }

    #[test]
    fn test_ad_driver_param_count() {
        let mut base = PortDriverBase::new("test", 1, PortFlags::default());
        let _ = ADDriverParams::create(&mut base).unwrap();
        // base params (~50) + ad-specific (~38) = ~88
        assert!(base.params.len() >= 80);
    }

    #[test]
    fn test_ad_driver_inherits_base() {
        let mut base = PortDriverBase::new("test", 1, PortFlags::default());
        let params = ADDriverParams::create(&mut base).unwrap();
        // Should be able to access base params through the base field
        assert!(base.find_param("ARRAY_COUNTER").is_some());
        assert_eq!(
            params.base.array_counter,
            base.find_param("ARRAY_COUNTER").unwrap()
        );
    }
}
