use std::collections::HashMap;
use std::sync::Arc;

use asyn_rs::adapter::AsynDeviceSupport;
use asyn_rs::port_handle::PortHandle;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, WriteCompletion};
use epics_base_rs::server::record::{Record, ScanType};

use ad_core_rs::plugin::registry::{ParamInfo, ParamRegistry, RegistryParamType};

use super::params::PointDetectorParams;

/// Build parameter registry for a PointDetector.
pub fn build_param_registry(params: &PointDetectorParams) -> ParamRegistry {
    let mut map: HashMap<String, ParamInfo> = HashMap::new();

    map.insert("MotorPos".into(), ParamInfo::float64(params.motor_pos, "MOTOR_POS"));
    map.insert("MotorPos_RBV".into(), ParamInfo::float64(params.motor_pos, "MOTOR_POS"));
    map.insert("BeamCurrent".into(), ParamInfo::float64(params.beam_current, "BEAM_CURRENT"));
    map.insert("BeamCurrent_RBV".into(), ParamInfo::float64(params.beam_current, "BEAM_CURRENT"));
    map.insert("ExposureTime".into(), ParamInfo::float64(params.exposure_time, "EXPOSURE_TIME"));
    map.insert("ExposureTime_RBV".into(), ParamInfo::float64(params.exposure_time, "EXPOSURE_TIME"));
    map.insert("DetValue_RBV".into(), ParamInfo::float64(params.det_value, "DET_VALUE"));
    map.insert("DetMode".into(), ParamInfo::int32(params.det_mode, "DET_MODE"));
    map.insert("DetMode_RBV".into(), ParamInfo::int32(params.det_mode, "DET_MODE"));
    map.insert("DetSigma".into(), ParamInfo::float64(params.det_sigma, "DET_SIGMA"));
    map.insert("DetSigma_RBV".into(), ParamInfo::float64(params.det_sigma, "DET_SIGMA"));
    map.insert("DetCenter".into(), ParamInfo::float64(params.det_center, "DET_CENTER"));
    map.insert("DetCenter_RBV".into(), ParamInfo::float64(params.det_center, "DET_CENTER"));

    map
}

/// Device support for PointDetector records.
pub struct PointDetectorDeviceSupport {
    inner: AsynDeviceSupport,
    registry: Arc<ParamRegistry>,
    dtyp_name: String,
}

impl PointDetectorDeviceSupport {
    pub fn from_handle(handle: PortHandle, registry: Arc<ParamRegistry>, dtyp: &str) -> Self {
        use asyn_rs::adapter::AsynLink;
        let link = AsynLink {
            port_name: String::new(),
            addr: 0,
            timeout: std::time::Duration::from_secs(1),
            drv_info: String::new(),
        };
        Self {
            inner: AsynDeviceSupport::from_handle(handle, link, "asynFloat64")
                .with_initial_readback(),
            registry,
            dtyp_name: dtyp.to_string(),
        }
    }
}

impl DeviceSupport for PointDetectorDeviceSupport {
    fn dtyp(&self) -> &str {
        &self.dtyp_name
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
            eprintln!("{}: no param mapping for suffix '{suffix}' (record: {name})", self.dtyp_name);
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

    fn io_intr_receiver(&mut self) -> Option<epics_base_rs::runtime::sync::mpsc::Receiver<()>> {
        self.inner.io_intr_receiver()
    }
}
