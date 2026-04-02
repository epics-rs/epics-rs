use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriverBase;

/// Parameters for asynNDArrayDriver base (file I/O, pool stats, array info, attributes).
#[derive(Clone, Copy)]
pub struct NDArrayDriverParams {
    // Detector info (Octet)
    pub port_name_self: usize,
    pub ad_core_version: usize,
    pub driver_version: usize,
    pub manufacturer: usize,
    pub model: usize,
    pub serial_number: usize,
    pub firmware_version: usize,
    pub sdk_version: usize,

    // Array info (Int32)
    pub array_size_x: usize,
    pub array_size_y: usize,
    pub array_size_z: usize,
    pub array_size: usize,
    pub array_counter: usize,
    pub array_callbacks: usize,
    pub n_dimensions: usize,
    pub array_dimensions: usize,
    pub data_type: usize,
    pub color_mode: usize,
    pub unique_id: usize,
    pub bayer_pattern: usize,
    pub codec: usize,
    pub compressed_size: usize,
    pub timestamp_rbv: usize,
    pub epics_ts_sec: usize,
    pub epics_ts_nsec: usize,

    // NDArray data (GenericPointer)
    pub ndarray_data: usize,

    // Pool stats
    pub pool_max_memory: usize,  // Float64 (MB)
    pub pool_used_memory: usize, // Float64 (MB)
    pub pool_alloc_buffers: usize,
    pub pool_free_buffers: usize,
    pub pool_max_buffers: usize,
    pub pool_pre_alloc: usize,
    pub pool_empty_free_list: usize,
    pub pool_poll_stats: usize,
    pub pool_num_pre_alloc_buffers: usize,

    // File I/O extras
    pub auto_save: usize,
    pub file_format: usize,
    pub free_capture: usize,

    // File I/O
    pub file_path: usize,
    pub file_name: usize,
    pub file_number: usize,
    pub file_template: usize,
    pub auto_increment: usize,
    pub full_file_name: usize,
    pub file_path_exists: usize,
    pub write_file: usize,
    pub read_file: usize,
    pub file_write_mode: usize,
    pub file_write_status: usize,
    pub file_write_message: usize,
    pub num_capture: usize,
    pub num_captured: usize,
    pub capture: usize,
    pub delete_driver_file: usize,
    pub lazy_open: usize,
    pub create_dir: usize,
    pub temp_suffix: usize,

    // Attributes
    pub attributes_file: usize,
    pub attributes_status: usize,
    pub attributes_macros: usize,

    // Queue
    pub num_queued_arrays: usize,

    // WaitForPlugins
    pub wait_for_plugins: usize,
}

impl NDArrayDriverParams {
    pub fn create(base: &mut PortDriverBase) -> AsynResult<Self> {
        Ok(Self {
            // Detector info
            port_name_self: base.create_param("PORT_NAME_SELF", ParamType::Octet)?,
            ad_core_version: base.create_param("ADCORE_VERSION", ParamType::Octet)?,
            driver_version: base.create_param("DRIVER_VERSION", ParamType::Octet)?,
            manufacturer: base.create_param("MANUFACTURER", ParamType::Octet)?,
            model: base.create_param("MODEL", ParamType::Octet)?,
            serial_number: base.create_param("SERIAL_NUMBER", ParamType::Octet)?,
            firmware_version: base.create_param("FIRMWARE_VERSION", ParamType::Octet)?,
            sdk_version: base.create_param("SDK_VERSION", ParamType::Octet)?,

            // Array info
            array_size_x: base.create_param("ARRAY_SIZE_X", ParamType::Int32)?,
            array_size_y: base.create_param("ARRAY_SIZE_Y", ParamType::Int32)?,
            array_size_z: base.create_param("ARRAY_SIZE_Z", ParamType::Int32)?,
            array_size: base.create_param("ARRAY_SIZE", ParamType::Int32)?,
            array_counter: base.create_param("ARRAY_COUNTER", ParamType::Int32)?,
            array_callbacks: base.create_param("ARRAY_CALLBACKS", ParamType::Int32)?,
            n_dimensions: base.create_param("ARRAY_NDIMENSIONS", ParamType::Int32)?,
            array_dimensions: base.create_param("ARRAY_DIMENSIONS", ParamType::Int32Array)?,
            data_type: base.create_param("DATA_TYPE", ParamType::Int32)?,
            color_mode: base.create_param("COLOR_MODE", ParamType::Int32)?,
            unique_id: base.create_param("UNIQUE_ID", ParamType::Int32)?,
            bayer_pattern: base.create_param("BAYER_PATTERN", ParamType::Int32)?,
            codec: base.create_param("CODEC", ParamType::Octet)?,
            compressed_size: base.create_param("COMPRESSED_SIZE", ParamType::Int32)?,
            timestamp_rbv: base.create_param("TIME_STAMP", ParamType::Float64)?,
            epics_ts_sec: base.create_param("EPICS_TS_SEC", ParamType::Int32)?,
            epics_ts_nsec: base.create_param("EPICS_TS_NSEC", ParamType::Int32)?,

            // NDArray data
            ndarray_data: base.create_param("ARRAY_DATA", ParamType::GenericPointer)?,

            // Pool stats
            pool_max_memory: base.create_param("POOL_MAX_MEMORY", ParamType::Float64)?,
            pool_used_memory: base.create_param("POOL_USED_MEMORY", ParamType::Float64)?,
            pool_alloc_buffers: base.create_param("POOL_ALLOC_BUFFERS", ParamType::Int32)?,
            pool_free_buffers: base.create_param("POOL_FREE_BUFFERS", ParamType::Int32)?,
            pool_max_buffers: base.create_param("POOL_MAX_BUFFERS", ParamType::Int32)?,
            pool_pre_alloc: base.create_param("POOL_PRE_ALLOC_BUFFERS", ParamType::Int32)?,
            pool_empty_free_list: base.create_param("POOL_EMPTY_FREELIST", ParamType::Int32)?,
            pool_poll_stats: base.create_param("POOL_POLL_STATS", ParamType::Int32)?,
            pool_num_pre_alloc_buffers: base
                .create_param("POOL_NUM_PRE_ALLOC_BUFFERS", ParamType::Int32)?,

            // File I/O extras
            auto_save: base.create_param("AUTO_SAVE", ParamType::Int32)?,
            file_format: base.create_param("FILE_FORMAT", ParamType::Int32)?,
            free_capture: base.create_param("FREE_CAPTURE", ParamType::Int32)?,

            // File I/O
            file_path: base.create_param("FILE_PATH", ParamType::Octet)?,
            file_name: base.create_param("FILE_NAME", ParamType::Octet)?,
            file_number: base.create_param("FILE_NUMBER", ParamType::Int32)?,
            file_template: base.create_param("FILE_TEMPLATE", ParamType::Octet)?,
            auto_increment: base.create_param("AUTO_INCREMENT", ParamType::Int32)?,
            full_file_name: base.create_param("FULL_FILE_NAME", ParamType::Octet)?,
            file_path_exists: base.create_param("FILE_PATH_EXISTS", ParamType::Int32)?,
            write_file: base.create_param("WRITE_FILE", ParamType::Int32)?,
            read_file: base.create_param("READ_FILE", ParamType::Int32)?,
            file_write_mode: base.create_param("WRITE_MODE", ParamType::Int32)?,
            file_write_status: base.create_param("WRITE_STATUS", ParamType::Int32)?,
            file_write_message: base.create_param("WRITE_MESSAGE", ParamType::Octet)?,
            num_capture: base.create_param("NUM_CAPTURE", ParamType::Int32)?,
            num_captured: base.create_param("NUM_CAPTURED", ParamType::Int32)?,
            capture: base.create_param("CAPTURE", ParamType::Int32)?,
            delete_driver_file: base.create_param("DELETE_DRIVER_FILE", ParamType::Int32)?,
            lazy_open: base.create_param("FILE_LAZY_OPEN", ParamType::Int32)?,
            create_dir: base.create_param("CREATE_DIR", ParamType::Int32)?,
            temp_suffix: base.create_param("FILE_TEMP_SUFFIX", ParamType::Octet)?,

            // Attributes
            attributes_file: base.create_param("ND_ATTRIBUTES_FILE", ParamType::Octet)?,
            attributes_status: base.create_param("ND_ATTRIBUTES_STATUS", ParamType::Int32)?,
            attributes_macros: base.create_param("ND_ATTRIBUTES_MACROS", ParamType::Octet)?,

            // Queue
            num_queued_arrays: base.create_param("NUM_QUEUED_ARRAYS", ParamType::Int32)?,

            // WaitForPlugins
            wait_for_plugins: base.create_param("WAIT_FOR_PLUGINS", ParamType::Int32)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asyn_rs::port::PortFlags;

    #[test]
    fn test_create_ndarray_driver_params() {
        let mut base = PortDriverBase::new("test", 1, PortFlags::default());
        let params = NDArrayDriverParams::create(&mut base).unwrap();
        assert!(base.find_param("ARRAY_COUNTER").is_some());
        assert!(base.find_param("FILE_PATH").is_some());
        assert!(base.find_param("POOL_MAX_MEMORY").is_some());
        assert!(base.find_param("ARRAY_DATA").is_some());
        assert!(base.find_param("ND_ATTRIBUTES_FILE").is_some());
        assert_eq!(
            params.array_counter,
            base.find_param("ARRAY_COUNTER").unwrap()
        );
    }

    #[test]
    fn test_ndarray_driver_param_count() {
        let mut base = PortDriverBase::new("test", 1, PortFlags::default());
        let _ = NDArrayDriverParams::create(&mut base).unwrap();
        // Should have created ~50 params
        assert!(base.params.len() >= 45);
    }
}
