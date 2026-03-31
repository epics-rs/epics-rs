use std::collections::HashMap;
use std::sync::Arc;

use asyn_rs::adapter::AsynDeviceSupport;
use asyn_rs::port_handle::PortHandle;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, WriteCompletion};
use epics_base_rs::server::record::{Record, ScanType};

use ad_core_rs::params::ADBaseParams;
use ad_core_rs::plugin::registry::{ParamInfo, ParamRegistry, RegistryParamType};

use super::params::MovingDotParams;

/// Build the parameter registry for MovingDot (ADBase + MovingDot-specific params).
pub fn build_param_registry(ad: &ADBaseParams, dot: &MovingDotParams) -> ParamRegistry {
    let mut map: HashMap<String, ParamInfo> = HashMap::new();
    let base = &ad.base;

    // ===== ADBase params =====

    // Image size
    map.insert("MaxSizeX_RBV".into(), ParamInfo::int32(ad.max_size_x, "MAX_SIZE_X"));
    map.insert("MaxSizeY_RBV".into(), ParamInfo::int32(ad.max_size_y, "MAX_SIZE_Y"));
    map.insert("SizeX".into(), ParamInfo::int32(ad.size_x, "SIZE_X"));
    map.insert("SizeX_RBV".into(), ParamInfo::int32(ad.size_x, "SIZE_X"));
    map.insert("SizeY".into(), ParamInfo::int32(ad.size_y, "SIZE_Y"));
    map.insert("SizeY_RBV".into(), ParamInfo::int32(ad.size_y, "SIZE_Y"));
    map.insert("MinX".into(), ParamInfo::int32(ad.min_x, "MIN_X"));
    map.insert("MinX_RBV".into(), ParamInfo::int32(ad.min_x, "MIN_X"));
    map.insert("MinY".into(), ParamInfo::int32(ad.min_y, "MIN_Y"));
    map.insert("MinY_RBV".into(), ParamInfo::int32(ad.min_y, "MIN_Y"));
    map.insert("BinX".into(), ParamInfo::int32(ad.bin_x, "BIN_X"));
    map.insert("BinX_RBV".into(), ParamInfo::int32(ad.bin_x, "BIN_X"));
    map.insert("BinY".into(), ParamInfo::int32(ad.bin_y, "BIN_Y"));
    map.insert("BinY_RBV".into(), ParamInfo::int32(ad.bin_y, "BIN_Y"));
    map.insert("ReverseX".into(), ParamInfo::int32(ad.reverse_x, "REVERSE_X"));
    map.insert("ReverseX_RBV".into(), ParamInfo::int32(ad.reverse_x, "REVERSE_X"));
    map.insert("ReverseY".into(), ParamInfo::int32(ad.reverse_y, "REVERSE_Y"));
    map.insert("ReverseY_RBV".into(), ParamInfo::int32(ad.reverse_y, "REVERSE_Y"));

    // Acquire control
    map.insert("Acquire".into(), ParamInfo::int32(ad.acquire, "ACQUIRE"));
    map.insert("Acquire_RBV".into(), ParamInfo::int32(ad.acquire, "ACQUIRE"));
    map.insert("ImageMode".into(), ParamInfo::int32(ad.image_mode, "IMAGE_MODE"));
    map.insert("ImageMode_RBV".into(), ParamInfo::int32(ad.image_mode, "IMAGE_MODE"));
    map.insert("NumImages".into(), ParamInfo::int32(ad.num_images, "NUM_IMAGES"));
    map.insert("NumImages_RBV".into(), ParamInfo::int32(ad.num_images, "NUM_IMAGES"));
    map.insert("NumImagesCounter_RBV".into(), ParamInfo::int32(ad.num_images_counter, "NUM_IMAGES_COUNTER"));
    map.insert("NumExposures".into(), ParamInfo::int32(ad.num_exposures, "NUM_EXPOSURES"));
    map.insert("NumExposures_RBV".into(), ParamInfo::int32(ad.num_exposures, "NUM_EXPOSURES"));
    map.insert("AcquireTime".into(), ParamInfo::float64(ad.acquire_time, "ACQUIRE_TIME"));
    map.insert("AcquireTime_RBV".into(), ParamInfo::float64(ad.acquire_time, "ACQUIRE_TIME"));
    map.insert("AcquirePeriod".into(), ParamInfo::float64(ad.acquire_period, "ACQUIRE_PERIOD"));
    map.insert("AcquirePeriod_RBV".into(), ParamInfo::float64(ad.acquire_period, "ACQUIRE_PERIOD"));
    map.insert("TimeRemaining_RBV".into(), ParamInfo::float64(ad.time_remaining, "TIME_REMAINING"));
    map.insert("Status_RBV".into(), ParamInfo::int32(ad.status, "DETECTOR_STATE"));
    map.insert("DetectorState_RBV".into(), ParamInfo::int32(ad.status, "DETECTOR_STATE"));
    map.insert("StatusMessage_RBV".into(), ParamInfo::string(ad.status_message, "STATUS_MESSAGE"));
    map.insert("AcquireBusy".into(), ParamInfo::int32(ad.acquire_busy, "ACQUIRE_BUSY"));
    map.insert("AcquireBusy_RBV".into(), ParamInfo::int32(ad.acquire_busy, "ACQUIRE_BUSY"));
    map.insert("WaitForPlugins".into(), ParamInfo::int32(ad.wait_for_plugins, "WAIT_FOR_PLUGINS"));
    map.insert("ReadStatus".into(), ParamInfo::int32(ad.read_status, "READ_STATUS"));

    // Detector
    map.insert("ADGain".into(), ParamInfo::float64(ad.gain, "GAIN"));
    map.insert("ADGain_RBV".into(), ParamInfo::float64(ad.gain, "GAIN"));
    map.insert("FrameType".into(), ParamInfo::int32(ad.frame_type, "FRAME_TYPE"));
    map.insert("FrameType_RBV".into(), ParamInfo::int32(ad.frame_type, "FRAME_TYPE"));
    map.insert("TriggerMode".into(), ParamInfo::int32(ad.trigger_mode, "TRIGGER_MODE"));
    map.insert("TriggerMode_RBV".into(), ParamInfo::int32(ad.trigger_mode, "TRIGGER_MODE"));

    // Shutter
    map.insert("ShutterControl".into(), ParamInfo::int32(ad.shutter_control, "SHUTTER_CONTROL"));
    map.insert("ShutterControl_RBV".into(), ParamInfo::int32(ad.shutter_control, "SHUTTER_CONTROL"));
    map.insert("ShutterControlEPICS".into(), ParamInfo::int32(ad.shutter_control_epics, "SHUTTER_CONTROL_EPICS"));
    map.insert("ShutterStatus_RBV".into(), ParamInfo::int32(ad.shutter_status, "SHUTTER_STATUS"));
    map.insert("ShutterStatusEPICS_RBV".into(), ParamInfo::int32(ad.shutter_status_epics, "SHUTTER_STATUS_EPICS"));
    map.insert("ShutterMode".into(), ParamInfo::int32(ad.shutter_mode, "SHUTTER_MODE"));
    map.insert("ShutterMode_RBV".into(), ParamInfo::int32(ad.shutter_mode, "SHUTTER_MODE"));
    map.insert("ShutterOpenDelay".into(), ParamInfo::float64(ad.shutter_open_delay, "SHUTTER_OPEN_DELAY"));
    map.insert("ShutterOpenDelay_RBV".into(), ParamInfo::float64(ad.shutter_open_delay, "SHUTTER_OPEN_DELAY"));
    map.insert("ShutterCloseDelay".into(), ParamInfo::float64(ad.shutter_close_delay, "SHUTTER_CLOSE_DELAY"));
    map.insert("ShutterCloseDelay_RBV".into(), ParamInfo::float64(ad.shutter_close_delay, "SHUTTER_CLOSE_DELAY"));

    // Temperature
    map.insert("Temperature".into(), ParamInfo::float64(ad.temperature, "TEMPERATURE"));
    map.insert("Temperature_RBV".into(), ParamInfo::float64(ad.temperature, "TEMPERATURE"));
    map.insert("TemperatureActual".into(), ParamInfo::float64(ad.temperature_actual, "TEMPERATURE_ACTUAL"));

    // Communication
    map.insert("StringToServer".into(), ParamInfo::string(ad.string_to_server, "STRING_TO_SERVER"));
    map.insert("StringToServer_RBV".into(), ParamInfo::string(ad.string_to_server, "STRING_TO_SERVER"));
    map.insert("StringFromServer_RBV".into(), ParamInfo::string(ad.string_from_server, "STRING_FROM_SERVER"));

    map.insert("AcquireBusyCB".into(), ParamInfo::int32(ad.acquire_busy, "ACQUIRE_BUSY"));

    // ===== NDArrayBase params =====

    map.insert("PortName_RBV".into(), ParamInfo::string(base.port_name_self, "PORT_NAME_SELF"));
    map.insert("ADCoreVersion_RBV".into(), ParamInfo::string(base.ad_core_version, "ADCORE_VERSION"));
    map.insert("DriverVersion_RBV".into(), ParamInfo::string(base.driver_version, "DRIVER_VERSION"));
    map.insert("Manufacturer_RBV".into(), ParamInfo::string(base.manufacturer, "MANUFACTURER"));
    map.insert("Model_RBV".into(), ParamInfo::string(base.model, "MODEL"));
    map.insert("SerialNumber_RBV".into(), ParamInfo::string(base.serial_number, "SERIAL_NUMBER"));
    map.insert("FirmwareVersion_RBV".into(), ParamInfo::string(base.firmware_version, "FIRMWARE_VERSION"));
    map.insert("SDKVersion_RBV".into(), ParamInfo::string(base.sdk_version, "SDK_VERSION"));

    map.insert("ArraySizeX_RBV".into(), ParamInfo::int32(base.array_size_x, "ARRAY_SIZE_X"));
    map.insert("ArraySizeY_RBV".into(), ParamInfo::int32(base.array_size_y, "ARRAY_SIZE_Y"));
    map.insert("ArraySizeZ_RBV".into(), ParamInfo::int32(base.array_size_z, "ARRAY_SIZE_Z"));
    map.insert("ArraySize_RBV".into(), ParamInfo::int32(base.array_size, "ARRAY_SIZE"));
    map.insert("ArraySize0_RBV".into(), ParamInfo::int32(base.array_size_x, "ARRAY_SIZE_X"));
    map.insert("ArraySize1_RBV".into(), ParamInfo::int32(base.array_size_y, "ARRAY_SIZE_Y"));
    map.insert("ArraySize2_RBV".into(), ParamInfo::int32(base.array_size_z, "ARRAY_SIZE_Z"));
    map.insert("ArrayCounter".into(), ParamInfo::int32(base.array_counter, "ARRAY_COUNTER"));
    map.insert("ArrayCounter_RBV".into(), ParamInfo::int32(base.array_counter, "ARRAY_COUNTER"));
    map.insert("ArrayCallbacks".into(), ParamInfo::int32(base.array_callbacks, "ARRAY_CALLBACKS"));
    map.insert("ArrayCallbacks_RBV".into(), ParamInfo::int32(base.array_callbacks, "ARRAY_CALLBACKS"));
    map.insert("NDimensions".into(), ParamInfo::int32(base.n_dimensions, "NDIMENSIONS"));
    map.insert("NDimensions_RBV".into(), ParamInfo::int32(base.n_dimensions, "NDIMENSIONS"));
    map.insert("DataType".into(), ParamInfo::int32(base.data_type, "DATA_TYPE"));
    map.insert("DataType_RBV".into(), ParamInfo::int32(base.data_type, "DATA_TYPE"));
    map.insert("ColorMode".into(), ParamInfo::int32(base.color_mode, "COLOR_MODE"));
    map.insert("ColorMode_RBV".into(), ParamInfo::int32(base.color_mode, "COLOR_MODE"));
    map.insert("UniqueId_RBV".into(), ParamInfo::int32(base.unique_id, "UNIQUE_ID"));
    map.insert("BayerPattern_RBV".into(), ParamInfo::int32(base.bayer_pattern, "BAYER_PATTERN"));
    map.insert("Codec_RBV".into(), ParamInfo::string(base.codec, "CODEC"));
    map.insert("CompressedSize_RBV".into(), ParamInfo::int32(base.compressed_size, "COMPRESSED_SIZE"));
    map.insert("TimeStamp_RBV".into(), ParamInfo::float64(base.timestamp_rbv, "TIMESTAMP"));
    map.insert("EpicsTSSec_RBV".into(), ParamInfo::int32(base.epics_ts_sec, "EPICS_TS_SEC"));
    map.insert("EpicsTSNsec_RBV".into(), ParamInfo::int32(base.epics_ts_nsec, "EPICS_TS_NSEC"));

    // Pool stats
    map.insert("PoolMaxMem".into(), ParamInfo::float64(base.pool_max_memory, "POOL_MAX_MEMORY"));
    map.insert("PoolMaxMem_RBV".into(), ParamInfo::float64(base.pool_max_memory, "POOL_MAX_MEMORY"));
    map.insert("PoolUsedMem".into(), ParamInfo::float64(base.pool_used_memory, "POOL_USED_MEMORY"));
    map.insert("PoolUsedMem_RBV".into(), ParamInfo::float64(base.pool_used_memory, "POOL_USED_MEMORY"));
    map.insert("PoolAllocBuffers".into(), ParamInfo::int32(base.pool_alloc_buffers, "POOL_ALLOC_BUFFERS"));
    map.insert("PoolAllocBuffers_RBV".into(), ParamInfo::int32(base.pool_alloc_buffers, "POOL_ALLOC_BUFFERS"));
    map.insert("PoolFreeBuffers".into(), ParamInfo::int32(base.pool_free_buffers, "POOL_FREE_BUFFERS"));
    map.insert("PoolFreeBuffers_RBV".into(), ParamInfo::int32(base.pool_free_buffers, "POOL_FREE_BUFFERS"));
    map.insert("PoolMaxBuffers_RBV".into(), ParamInfo::int32(base.pool_max_buffers, "POOL_MAX_BUFFERS"));
    map.insert("PoolPreAlloc".into(), ParamInfo::int32(base.pool_pre_alloc, "POOL_PRE_ALLOC"));
    map.insert("PoolEmptyFreeList".into(), ParamInfo::int32(base.pool_empty_free_list, "POOL_EMPTY_FREE_LIST"));
    map.insert("EmptyFreeList".into(), ParamInfo::int32(base.pool_empty_free_list, "POOL_EMPTY_FREE_LIST"));
    map.insert("PoolPollStats".into(), ParamInfo::int32(base.pool_poll_stats, "POOL_POLL_STATS"));

    // NDFile params
    map.insert("FilePath".into(), ParamInfo::string(base.file_path, "FILE_PATH"));
    map.insert("FilePath_RBV".into(), ParamInfo::string(base.file_path, "FILE_PATH"));
    map.insert("FileName".into(), ParamInfo::string(base.file_name, "FILE_NAME"));
    map.insert("FileName_RBV".into(), ParamInfo::string(base.file_name, "FILE_NAME"));
    map.insert("FileNumber".into(), ParamInfo::int32(base.file_number, "FILE_NUMBER"));
    map.insert("FileNumber_RBV".into(), ParamInfo::int32(base.file_number, "FILE_NUMBER"));
    map.insert("FileTemplate".into(), ParamInfo::string(base.file_template, "FILE_TEMPLATE"));
    map.insert("FileTemplate_RBV".into(), ParamInfo::string(base.file_template, "FILE_TEMPLATE"));
    map.insert("AutoIncrement".into(), ParamInfo::int32(base.auto_increment, "AUTO_INCREMENT"));
    map.insert("AutoIncrement_RBV".into(), ParamInfo::int32(base.auto_increment, "AUTO_INCREMENT"));
    map.insert("FullFileName_RBV".into(), ParamInfo::string(base.full_file_name, "FULL_FILE_NAME"));
    map.insert("FilePathExists_RBV".into(), ParamInfo::int32(base.file_path_exists, "FILE_PATH_EXISTS"));
    map.insert("WriteFile".into(), ParamInfo::int32(base.write_file, "WRITE_FILE"));
    map.insert("WriteFile_RBV".into(), ParamInfo::int32(base.write_file, "WRITE_FILE"));
    map.insert("ReadFile".into(), ParamInfo::int32(base.read_file, "READ_FILE"));
    map.insert("ReadFile_RBV".into(), ParamInfo::int32(base.read_file, "READ_FILE"));
    map.insert("FileWriteMode".into(), ParamInfo::int32(base.file_write_mode, "FILE_WRITE_MODE"));
    map.insert("FileWriteMode_RBV".into(), ParamInfo::int32(base.file_write_mode, "FILE_WRITE_MODE"));
    map.insert("FileWriteStatus_RBV".into(), ParamInfo::int32(base.file_write_status, "FILE_WRITE_STATUS"));
    map.insert("FileWriteMessage_RBV".into(), ParamInfo::string(base.file_write_message, "FILE_WRITE_MESSAGE"));
    map.insert("NumCapture".into(), ParamInfo::int32(base.num_capture, "NUM_CAPTURE"));
    map.insert("NumCapture_RBV".into(), ParamInfo::int32(base.num_capture, "NUM_CAPTURE"));
    map.insert("NumCaptured_RBV".into(), ParamInfo::int32(base.num_captured, "NUM_CAPTURED"));
    map.insert("Capture".into(), ParamInfo::int32(base.capture, "CAPTURE"));
    map.insert("Capture_RBV".into(), ParamInfo::int32(base.capture, "CAPTURE"));
    map.insert("DeleteDriverFile".into(), ParamInfo::int32(base.delete_driver_file, "DELETE_DRIVER_FILE"));
    map.insert("DeleteDriverFile_RBV".into(), ParamInfo::int32(base.delete_driver_file, "DELETE_DRIVER_FILE"));
    map.insert("LazyOpen".into(), ParamInfo::int32(base.lazy_open, "LAZY_OPEN"));
    map.insert("LazyOpen_RBV".into(), ParamInfo::int32(base.lazy_open, "LAZY_OPEN"));
    map.insert("CreateDir".into(), ParamInfo::int32(base.create_dir, "CREATE_DIR"));
    map.insert("CreateDir_RBV".into(), ParamInfo::int32(base.create_dir, "CREATE_DIR"));
    map.insert("TempSuffix".into(), ParamInfo::string(base.temp_suffix, "TEMP_SUFFIX"));
    map.insert("TempSuffix_RBV".into(), ParamInfo::string(base.temp_suffix, "TEMP_SUFFIX"));
    map.insert("AutoSave".into(), ParamInfo::int32(base.auto_save, "AUTO_SAVE"));
    map.insert("AutoSave_RBV".into(), ParamInfo::int32(base.auto_save, "AUTO_SAVE"));
    map.insert("FileFormat".into(), ParamInfo::int32(base.file_format, "FILE_FORMAT"));
    map.insert("FileFormat_RBV".into(), ParamInfo::int32(base.file_format, "FILE_FORMAT"));
    map.insert("NDAttributesFile".into(), ParamInfo::string(base.attributes_file, "ATTRIBUTES_FILE"));

    // ===== MovingDot-specific params =====
    map.insert("MotorXPos".into(), ParamInfo::float64(dot.motor_x_pos, "DOT_MOTOR_X_POS"));
    map.insert("MotorXPos_RBV".into(), ParamInfo::float64(dot.motor_x_pos, "DOT_MOTOR_X_POS"));
    map.insert("MotorYPos".into(), ParamInfo::float64(dot.motor_y_pos, "DOT_MOTOR_Y_POS"));
    map.insert("MotorYPos_RBV".into(), ParamInfo::float64(dot.motor_y_pos, "DOT_MOTOR_Y_POS"));
    map.insert("BeamCurrent".into(), ParamInfo::float64(dot.beam_current, "DOT_BEAM_CURRENT"));
    map.insert("BeamCurrent_RBV".into(), ParamInfo::float64(dot.beam_current, "DOT_BEAM_CURRENT"));
    map.insert("ShutterOpen".into(), ParamInfo::int32(dot.shutter_open, "DOT_SHUTTER_OPEN"));
    map.insert("ShutterOpen_RBV".into(), ParamInfo::int32(dot.shutter_open, "DOT_SHUTTER_OPEN"));

    map
}

/// Device support bridge for MovingDot area detector records.
pub struct MovingDotDeviceSupport {
    inner: AsynDeviceSupport,
    registry: Arc<ParamRegistry>,
}

impl MovingDotDeviceSupport {
    pub fn from_handle(handle: PortHandle, registry: Arc<ParamRegistry>) -> Self {
        use asyn_rs::adapter::AsynLink;
        let link = AsynLink {
            port_name: String::new(),
            addr: 0,
            timeout: std::time::Duration::from_secs(1),
            drv_info: String::new(),
        };
        Self {
            inner: AsynDeviceSupport::from_handle(handle, link, "asynInt32")
                .with_initial_readback(),
            registry,
        }
    }
}

impl DeviceSupport for MovingDotDeviceSupport {
    fn dtyp(&self) -> &str {
        "asynMovingDot"
    }

    fn set_record_info(&mut self, name: &str, scan: ScanType) {
        let suffix = name.rsplit(':').next().unwrap_or(name);
        if let Some(info) = self.registry.get(suffix) {
            self.inner.set_drv_info(&info.drv_info);
            self.inner.set_reason(info.param_index);
            let iface = match info.param_type {
                RegistryParamType::Int32 => "asynInt32",
                RegistryParamType::Float64 => "asynFloat64",
                RegistryParamType::Float64Array => "asynFloat64Array",
                RegistryParamType::OctetString => "asynOctet",
            };
            self.inner.set_iface_type(iface);
        } else {
            eprintln!("asynMovingDot: no param mapping for suffix '{suffix}' (record: {name})");
        }
        self.inner.set_record_info(name, scan);
    }

    fn init(&mut self, record: &mut dyn Record) -> CaResult<()> {
        self.inner.init(record)
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        self.inner.read(record)
    }

    fn write(&mut self, record: &mut dyn Record) -> CaResult<()> {
        self.inner.write(record)
    }

    fn write_begin(&mut self, record: &mut dyn Record) -> CaResult<Option<Box<dyn WriteCompletion>>> {
        self.inner.write_begin(record)
    }

    fn last_alarm(&self) -> Option<(u16, u16)> {
        self.inner.last_alarm()
    }

    fn last_timestamp(&self) -> Option<std::time::SystemTime> {
        self.inner.last_timestamp()
    }

    fn io_intr_receiver(&mut self) -> Option<tokio::sync::mpsc::Receiver<()>> {
        self.inner.io_intr_receiver()
    }
}
