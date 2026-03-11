use std::collections::HashMap;
use std::sync::Arc;

use asyn_rs::adapter::AsynDeviceSupport;
use asyn_rs::port_handle::PortHandle;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceSupport, WriteCompletion};
use epics_base_rs::server::iocsh::registry::*;
use epics_base_rs::server::record::{Record, ScanType};

use ad_core::ioc::GenericDriverContext;
use ad_core::params::ADBaseParams;
use ad_core::plugin::channel::NDArrayOutput;
use ad_core::plugin::registry::{ParamInfo, RegistryParamType};
use crate::driver::{SimDetectorRuntime, create_sim_detector};
use crate::params::SimDetectorParams;
use crate::SimDetector;

// Re-export for public API compatibility.
pub use ad_core::plugin::registry::{ParamRegistry, self};

/// Build the parameter registry from a SimDetector instance.
pub fn build_param_registry(det: &SimDetector) -> ParamRegistry {
    build_param_registry_from_params(&det.ad.params, &det.sim_params)
}

/// Build the parameter registry from param indices (for use after driver is consumed).
pub fn build_param_registry_from_params(ad: &ADBaseParams, sim: &SimDetectorParams) -> ParamRegistry {
    let mut map = HashMap::new();
    let base = &ad.base;

    // ===== ADBase.db params =====

    // Image size (Int32)
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
    map.insert("NumExposuresCounter_RBV".into(), ParamInfo::int32(ad.num_exposures_counter, "NUM_EXPOSURES_COUNTER"));
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

    // Detector (ADGain is the ADDriver-level gain, distinct from sim Gain)
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

    // AcquireBusyCB
    map.insert("AcquireBusyCB".into(), ParamInfo::int32(ad.acquire_busy, "ACQUIRE_BUSY"));

    // ===== NDArrayBase.db params =====

    // Detector info (string)
    map.insert("PortName_RBV".into(), ParamInfo::string(base.port_name_self, "PORT_NAME_SELF"));
    map.insert("ADCoreVersion_RBV".into(), ParamInfo::string(base.ad_core_version, "ADCORE_VERSION"));
    map.insert("DriverVersion_RBV".into(), ParamInfo::string(base.driver_version, "DRIVER_VERSION"));
    map.insert("Manufacturer_RBV".into(), ParamInfo::string(base.manufacturer, "MANUFACTURER"));
    map.insert("Model_RBV".into(), ParamInfo::string(base.model, "MODEL"));
    map.insert("SerialNumber_RBV".into(), ParamInfo::string(base.serial_number, "SERIAL_NUMBER"));
    map.insert("FirmwareVersion_RBV".into(), ParamInfo::string(base.firmware_version, "FIRMWARE_VERSION"));
    map.insert("SDKVersion_RBV".into(), ParamInfo::string(base.sdk_version, "SDK_VERSION"));

    // Array info (Int32)
    map.insert("ArraySizeX_RBV".into(), ParamInfo::int32(base.array_size_x, "ARRAY_SIZE_X"));
    map.insert("ArraySizeY_RBV".into(), ParamInfo::int32(base.array_size_y, "ARRAY_SIZE_Y"));
    map.insert("ArraySizeZ_RBV".into(), ParamInfo::int32(base.array_size_z, "ARRAY_SIZE_Z"));
    map.insert("ArraySize_RBV".into(), ParamInfo::int32(base.array_size, "ARRAY_SIZE"));
    map.insert("ArrayCounter".into(), ParamInfo::int32(base.array_counter, "ARRAY_COUNTER"));
    map.insert("ArrayCounter_RBV".into(), ParamInfo::int32(base.array_counter, "ARRAY_COUNTER"));
    map.insert("ArrayCallbacks".into(), ParamInfo::int32(base.array_callbacks, "ARRAY_CALLBACKS"));
    map.insert("ArrayCallbacks_RBV".into(), ParamInfo::int32(base.array_callbacks, "ARRAY_CALLBACKS"));
    map.insert("NDimensions".into(), ParamInfo::int32(base.n_dimensions, "NDIMENSIONS"));
    map.insert("NDimensions_RBV".into(), ParamInfo::int32(base.n_dimensions, "NDIMENSIONS"));
    map.insert("Dimensions".into(), ParamInfo::int32(base.n_dimensions, "NDIMENSIONS"));
    map.insert("Dimensions_RBV".into(), ParamInfo::int32(base.n_dimensions, "NDIMENSIONS"));
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

    // Pool stats — PoolMaxMem/PoolUsedMem are Float64 (MB), rest are Int32
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
    map.insert("PreAllocBuffers".into(), ParamInfo::int32(base.pool_pre_alloc, "POOL_PRE_ALLOC"));
    map.insert("NumPreAllocBuffers".into(), ParamInfo::int32(base.pool_num_pre_alloc_buffers, "POOL_NUM_PRE_ALLOC_BUFFERS"));
    map.insert("NumPreAllocBuffers_RBV".into(), ParamInfo::int32(base.pool_num_pre_alloc_buffers, "POOL_NUM_PRE_ALLOC_BUFFERS"));
    map.insert("NumQueuedArrays".into(), ParamInfo::int32(base.num_queued_arrays, "NUM_QUEUED_ARRAYS"));
    map.insert("NumQueuedArrays_RBV".into(), ParamInfo::int32(base.num_queued_arrays, "NUM_QUEUED_ARRAYS"));

    // Attributes
    map.insert("NDAttributesFile".into(), ParamInfo::string(base.attributes_file, "ATTRIBUTES_FILE"));
    map.insert("NDAttributesStatus".into(), ParamInfo::int32(base.attributes_status, "ATTRIBUTES_STATUS"));
    map.insert("NDAttributesStatus_RBV".into(), ParamInfo::int32(base.attributes_status, "ATTRIBUTES_STATUS"));
    map.insert("NDAttributesMacros".into(), ParamInfo::string(base.attributes_macros, "ATTRIBUTES_MACROS"));

    // ===== NDFile.db params =====

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
    map.insert("FreeCapture".into(), ParamInfo::int32(base.free_capture, "FREE_CAPTURE"));
    map.insert("CreateDirectory".into(), ParamInfo::int32(base.create_dir, "CREATE_DIR"));
    map.insert("CreateDirectory_RBV".into(), ParamInfo::int32(base.create_dir, "CREATE_DIR"));
    map.insert("WriteStatus".into(), ParamInfo::int32(base.file_write_status, "FILE_WRITE_STATUS"));
    map.insert("WriteMessage".into(), ParamInfo::string(base.file_write_message, "FILE_WRITE_MESSAGE"));

    // ===== simDetector.db params =====

    // Sim gains (Float64)
    map.insert("Gain".into(), ParamInfo::float64(sim.gain, "AD_GAIN"));
    map.insert("Gain_RBV".into(), ParamInfo::float64(sim.gain, "AD_GAIN"));
    map.insert("GainX".into(), ParamInfo::float64(sim.gain_x, "SIM_GAIN_X"));
    map.insert("GainX_RBV".into(), ParamInfo::float64(sim.gain_x, "SIM_GAIN_X"));
    map.insert("GainY".into(), ParamInfo::float64(sim.gain_y, "SIM_GAIN_Y"));
    map.insert("GainY_RBV".into(), ParamInfo::float64(sim.gain_y, "SIM_GAIN_Y"));
    map.insert("GainRed".into(), ParamInfo::float64(sim.gain_red, "SIM_GAIN_RED"));
    map.insert("GainRed_RBV".into(), ParamInfo::float64(sim.gain_red, "SIM_GAIN_RED"));
    map.insert("GainGreen".into(), ParamInfo::float64(sim.gain_green, "SIM_GAIN_GREEN"));
    map.insert("GainGreen_RBV".into(), ParamInfo::float64(sim.gain_green, "SIM_GAIN_GREEN"));
    map.insert("GainBlue".into(), ParamInfo::float64(sim.gain_blue, "SIM_GAIN_BLUE"));
    map.insert("GainBlue_RBV".into(), ParamInfo::float64(sim.gain_blue, "SIM_GAIN_BLUE"));
    map.insert("Offset".into(), ParamInfo::float64(sim.offset, "SIM_OFFSET"));
    map.insert("Offset_RBV".into(), ParamInfo::float64(sim.offset, "SIM_OFFSET"));
    map.insert("Noise".into(), ParamInfo::float64(sim.noise, "SIM_NOISE"));
    map.insert("Noise_RBV".into(), ParamInfo::float64(sim.noise, "SIM_NOISE"));
    map.insert("PeakHeightVariation".into(), ParamInfo::float64(sim.peak_height_variation, "SIM_PEAK_HEIGHT_VARIATION"));
    map.insert("PeakVariation".into(), ParamInfo::float64(sim.peak_height_variation, "SIM_PEAK_HEIGHT_VARIATION"));
    map.insert("PeakVariation_RBV".into(), ParamInfo::float64(sim.peak_height_variation, "SIM_PEAK_HEIGHT_VARIATION"));

    // Sim Int32
    map.insert("SimMode".into(), ParamInfo::int32(sim.sim_mode, "SIM_MODE"));
    map.insert("SimMode_RBV".into(), ParamInfo::int32(sim.sim_mode, "SIM_MODE"));
    map.insert("ResetImage".into(), ParamInfo::int32(sim.reset_image, "RESET_IMAGE"));
    map.insert("Reset".into(), ParamInfo::int32(sim.reset_image, "RESET_IMAGE"));
    map.insert("Reset_RBV".into(), ParamInfo::int32(sim.reset_image, "RESET_IMAGE"));
    map.insert("PeakStartX".into(), ParamInfo::int32(sim.peak_start_x, "SIM_PEAK_START_X"));
    map.insert("PeakStartX_RBV".into(), ParamInfo::int32(sim.peak_start_x, "SIM_PEAK_START_X"));
    map.insert("PeakStartY".into(), ParamInfo::int32(sim.peak_start_y, "SIM_PEAK_START_Y"));
    map.insert("PeakStartY_RBV".into(), ParamInfo::int32(sim.peak_start_y, "SIM_PEAK_START_Y"));
    map.insert("PeakWidthX".into(), ParamInfo::int32(sim.peak_width_x, "SIM_PEAK_WIDTH_X"));
    map.insert("PeakWidthX_RBV".into(), ParamInfo::int32(sim.peak_width_x, "SIM_PEAK_WIDTH_X"));
    map.insert("PeakWidthY".into(), ParamInfo::int32(sim.peak_width_y, "SIM_PEAK_WIDTH_Y"));
    map.insert("PeakWidthY_RBV".into(), ParamInfo::int32(sim.peak_width_y, "SIM_PEAK_WIDTH_Y"));
    map.insert("PeakNumX".into(), ParamInfo::int32(sim.peak_num_x, "SIM_PEAK_NUM_X"));
    map.insert("PeakNumX_RBV".into(), ParamInfo::int32(sim.peak_num_x, "SIM_PEAK_NUM_X"));
    map.insert("PeakNumY".into(), ParamInfo::int32(sim.peak_num_y, "SIM_PEAK_NUM_Y"));
    map.insert("PeakNumY_RBV".into(), ParamInfo::int32(sim.peak_num_y, "SIM_PEAK_NUM_Y"));
    map.insert("PeakStepX".into(), ParamInfo::int32(sim.peak_step_x, "SIM_PEAK_STEP_X"));
    map.insert("PeakStepX_RBV".into(), ParamInfo::int32(sim.peak_step_x, "SIM_PEAK_STEP_X"));
    map.insert("PeakStepY".into(), ParamInfo::int32(sim.peak_step_y, "SIM_PEAK_STEP_Y"));
    map.insert("PeakStepY_RBV".into(), ParamInfo::int32(sim.peak_step_y, "SIM_PEAK_STEP_Y"));

    // Sine params (Float64 + Int32) — write and readback
    map.insert("XSineOperation".into(), ParamInfo::int32(sim.x_sine_operation, "SIM_XSINE_OPERATION"));
    map.insert("XSineOperation_RBV".into(), ParamInfo::int32(sim.x_sine_operation, "SIM_XSINE_OPERATION"));
    map.insert("XSine1Amplitude".into(), ParamInfo::float64(sim.x_sine1_amplitude, "SIM_XSINE1_AMPLITUDE"));
    map.insert("XSine1Amplitude_RBV".into(), ParamInfo::float64(sim.x_sine1_amplitude, "SIM_XSINE1_AMPLITUDE"));
    map.insert("XSine1Frequency".into(), ParamInfo::float64(sim.x_sine1_frequency, "SIM_XSINE1_FREQUENCY"));
    map.insert("XSine1Frequency_RBV".into(), ParamInfo::float64(sim.x_sine1_frequency, "SIM_XSINE1_FREQUENCY"));
    map.insert("XSine1Phase".into(), ParamInfo::float64(sim.x_sine1_phase, "SIM_XSINE1_PHASE"));
    map.insert("XSine1Phase_RBV".into(), ParamInfo::float64(sim.x_sine1_phase, "SIM_XSINE1_PHASE"));
    map.insert("XSine2Amplitude".into(), ParamInfo::float64(sim.x_sine2_amplitude, "SIM_XSINE2_AMPLITUDE"));
    map.insert("XSine2Amplitude_RBV".into(), ParamInfo::float64(sim.x_sine2_amplitude, "SIM_XSINE2_AMPLITUDE"));
    map.insert("XSine2Frequency".into(), ParamInfo::float64(sim.x_sine2_frequency, "SIM_XSINE2_FREQUENCY"));
    map.insert("XSine2Frequency_RBV".into(), ParamInfo::float64(sim.x_sine2_frequency, "SIM_XSINE2_FREQUENCY"));
    map.insert("XSine2Phase".into(), ParamInfo::float64(sim.x_sine2_phase, "SIM_XSINE2_PHASE"));
    map.insert("XSine2Phase_RBV".into(), ParamInfo::float64(sim.x_sine2_phase, "SIM_XSINE2_PHASE"));
    map.insert("YSineOperation".into(), ParamInfo::int32(sim.y_sine_operation, "SIM_YSINE_OPERATION"));
    map.insert("YSineOperation_RBV".into(), ParamInfo::int32(sim.y_sine_operation, "SIM_YSINE_OPERATION"));
    map.insert("YSine1Amplitude".into(), ParamInfo::float64(sim.y_sine1_amplitude, "SIM_YSINE1_AMPLITUDE"));
    map.insert("YSine1Amplitude_RBV".into(), ParamInfo::float64(sim.y_sine1_amplitude, "SIM_YSINE1_AMPLITUDE"));
    map.insert("YSine1Frequency".into(), ParamInfo::float64(sim.y_sine1_frequency, "SIM_YSINE1_FREQUENCY"));
    map.insert("YSine1Frequency_RBV".into(), ParamInfo::float64(sim.y_sine1_frequency, "SIM_YSINE1_FREQUENCY"));
    map.insert("YSine1Phase".into(), ParamInfo::float64(sim.y_sine1_phase, "SIM_YSINE1_PHASE"));
    map.insert("YSine1Phase_RBV".into(), ParamInfo::float64(sim.y_sine1_phase, "SIM_YSINE1_PHASE"));
    map.insert("YSine2Amplitude".into(), ParamInfo::float64(sim.y_sine2_amplitude, "SIM_YSINE2_AMPLITUDE"));
    map.insert("YSine2Amplitude_RBV".into(), ParamInfo::float64(sim.y_sine2_amplitude, "SIM_YSINE2_AMPLITUDE"));
    map.insert("YSine2Frequency".into(), ParamInfo::float64(sim.y_sine2_frequency, "SIM_YSINE2_FREQUENCY"));
    map.insert("YSine2Frequency_RBV".into(), ParamInfo::float64(sim.y_sine2_frequency, "SIM_YSINE2_FREQUENCY"));
    map.insert("YSine2Phase".into(), ParamInfo::float64(sim.y_sine2_phase, "SIM_YSINE2_PHASE"));
    map.insert("YSine2Phase_RBV".into(), ParamInfo::float64(sim.y_sine2_phase, "SIM_YSINE2_PHASE"));

    map
}

/// Device support bridge between epics-base-rs records and SimDetector.
/// Wraps AsynDeviceSupport for PortHandle-based access.
pub struct SimDeviceSupport {
    inner: AsynDeviceSupport,
    registry: Arc<ParamRegistry>,
}

impl SimDeviceSupport {
    /// Create from a legacy `Arc<Mutex<SimDetector>>` (direct locking).
    pub fn new(
        driver: Arc<parking_lot::Mutex<SimDetector>>,
        registry: Arc<ParamRegistry>,
    ) -> Self {
        use asyn_rs::adapter::AsynLink;
        let link = AsynLink {
            port_name: String::new(),
            addr: 0,
            timeout: std::time::Duration::from_secs(1),
            drv_info: String::new(),
        };
        Self {
            inner: AsynDeviceSupport::new(driver, link, "asynInt32"),
            registry,
        }
    }

    /// Create from a [`PortHandle`] (actor model).
    pub fn from_handle(
        handle: PortHandle,
        registry: Arc<ParamRegistry>,
    ) -> Self {
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

impl DeviceSupport for SimDeviceSupport {
    fn dtyp(&self) -> &str {
        "asynSimDetector"
    }

    fn set_record_info(&mut self, name: &str, scan: ScanType) {
        // Extract suffix after last ':'
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
            eprintln!("asynSimDetector: no param mapping for record suffix '{suffix}' (record: {name})");
        }
        self.inner.set_record_info(name, scan);
    }

    fn init(&mut self, record: &mut dyn Record) -> CaResult<()> {
        self.inner.init(record)
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<()> {
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

// ============================================================================
// IOC registration
// ============================================================================

/// Register the SimDetector configure command and device support on an [`AdIoc`].
///
/// After calling this, `simDetectorConfig(...)` can be used in st.cmd to
/// create a SimDetector, and records with `DTYP=asynSimDetector` will be
/// wired automatically.
pub fn register(ioc: &mut ad_plugins::ioc::AdIoc) {
    epics_base_rs::runtime::env::set_default("ADSIMDETECTOR", env!("CARGO_MANIFEST_DIR"));

    let driver_handle: Arc<std::sync::Mutex<Option<PortHandle>>> =
        Arc::new(std::sync::Mutex::new(None));
    let driver_registry: Arc<std::sync::Mutex<Option<Arc<ParamRegistry>>>> =
        Arc::new(std::sync::Mutex::new(None));
    // Keep runtime alive for the IOC's lifetime.
    let driver_runtime: Arc<std::sync::Mutex<Option<SimDetectorRuntime>>> =
        Arc::new(std::sync::Mutex::new(None));

    // --- simDetectorConfig startup command ---
    {
        let mgr = ioc.mgr().clone();
        let trace = ioc.trace().clone();
        let ph = driver_handle.clone();
        let reg = driver_registry.clone();
        let rt = driver_runtime.clone();
        ioc.register_startup_command(CommandDef::new(
            "simDetectorConfig",
            vec![
                ArgDesc { name: "portName", arg_type: ArgType::String, optional: false },
                ArgDesc { name: "sizeX", arg_type: ArgType::Int, optional: true },
                ArgDesc { name: "sizeY", arg_type: ArgType::Int, optional: true },
                ArgDesc { name: "maxMemory", arg_type: ArgType::Int, optional: true },
            ],
            "simDetectorConfig portName [sizeX] [sizeY] [maxMemory]",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let port_name = match &args[0] {
                    ArgValue::String(s) => s.clone(),
                    _ => return Err("portName required".into()),
                };
                let size_x = match args.get(1) { Some(ArgValue::Int(n)) => *n as i32, _ => 256 };
                let size_y = match args.get(2) { Some(ArgValue::Int(n)) => *n as i32, _ => 256 };
                let max_memory = match args.get(3) { Some(ArgValue::Int(n)) => *n as usize, _ => 50_000_000 };

                println!("simDetectorConfig: port={port_name}, size={size_x}x{size_y}, maxMemory={max_memory}");

                let runtime = create_sim_detector(&port_name, size_x, size_y, max_memory, NDArrayOutput::new())
                    .map_err(|e| format!("failed to create SimDetector: {e}"))?;

                let registry = Arc::new(build_param_registry_from_params(&runtime.ad_params, &runtime.sim_params));
                let port_handle = runtime.port_handle().clone();

                asyn_rs::asyn_record::register_port(&port_name, port_handle.clone(), trace.clone());

                mgr.set_driver(Arc::new(GenericDriverContext::new(
                    runtime.pool().clone(),
                    runtime.array_output().clone(),
                    &port_name,
                )));

                *ph.lock().unwrap() = Some(port_handle);
                *reg.lock().unwrap() = Some(registry);
                *rt.lock().unwrap() = Some(runtime);

                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- asynSimDetector device support ---
    {
        let ph = driver_handle;
        let reg = driver_registry;
        ioc.register_device_support("asynSimDetector", move || {
            let handle = ph.lock().unwrap()
                .as_ref().expect("simDetectorConfig must be called before iocInit")
                .clone();
            let registry = reg.lock().unwrap()
                .as_ref().expect("simDetectorConfig must be called before iocInit")
                .clone();
            Box::new(SimDeviceSupport::from_handle(handle, registry))
        });
    }
}
