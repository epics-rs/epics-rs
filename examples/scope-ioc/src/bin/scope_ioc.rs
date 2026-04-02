//! Scope Simulator IOC — st.cmd-style startup using IocApplication.
//!
//! Port of EPICS testAsynPortDriver as an IOC with Channel Access.
//!
//! Usage:
//!   cargo run --release -p scope-ioc --features ioc --bin scope_ioc -- ioc/st.cmd

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use epics_base_rs::runtime::sync::Notify;

use scope_ioc::driver::*;
use asyn_rs::interrupt::{InterruptFilter, InterruptSubscription};
use asyn_rs::port::PortDriver;
use asyn_rs::user::AsynUser;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport};
use epics_ca_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::registry::*;
use epics_base_rs::server::record::{Record, ScanType};
use epics_base_rs::types::EpicsValue;

// ========== IOC Device Support ==========

#[derive(Clone, Copy)]
enum ScopeParamType {
    Enum,
    Int32,
    Float64,
    Float64Array,
}

#[derive(Clone, Copy)]
struct ScopeParamInfo {
    param_index: usize,
    param_type: ScopeParamType,
}

impl ScopeParamInfo {
    fn enumerated(idx: usize) -> Self { Self { param_index: idx, param_type: ScopeParamType::Enum } }
    fn int32(idx: usize) -> Self { Self { param_index: idx, param_type: ScopeParamType::Int32 } }
    fn float64(idx: usize) -> Self { Self { param_index: idx, param_type: ScopeParamType::Float64 } }
    fn float64_array(idx: usize) -> Self { Self { param_index: idx, param_type: ScopeParamType::Float64Array } }
}

type ScopeParamRegistry = HashMap<String, ScopeParamInfo>;

fn build_param_registry(drv: &ScopeSimulator) -> ScopeParamRegistry {
    let mut m = HashMap::new();

    // Control params (output records)
    m.insert("Run".into(), ScopeParamInfo::enumerated(drv.p_run));
    m.insert("UpdateTime".into(), ScopeParamInfo::float64(drv.p_update_time));
    m.insert("VoltOffset".into(), ScopeParamInfo::float64(drv.p_volt_offset));
    m.insert("TriggerDelay".into(), ScopeParamInfo::float64(drv.p_trigger_delay));
    m.insert("NoiseAmplitude".into(), ScopeParamInfo::float64(drv.p_noise_amplitude));
    m.insert("TimePerDivSelect".into(), ScopeParamInfo::enumerated(drv.p_time_per_div_select));
    m.insert("VertGainSelect".into(), ScopeParamInfo::enumerated(drv.p_vert_gain_select));
    m.insert("VoltsPerDivSelect".into(), ScopeParamInfo::enumerated(drv.p_volts_per_div_select));

    // Readback params (input records)
    m.insert("Run_RBV".into(), ScopeParamInfo::enumerated(drv.p_run));
    m.insert("MaxPoints_RBV".into(), ScopeParamInfo::int32(drv.p_max_points));
    m.insert("UpdateTime_RBV".into(), ScopeParamInfo::float64(drv.p_update_time));
    m.insert("VoltOffset_RBV".into(), ScopeParamInfo::float64(drv.p_volt_offset));
    m.insert("TriggerDelay_RBV".into(), ScopeParamInfo::float64(drv.p_trigger_delay));
    m.insert("NoiseAmplitude_RBV".into(), ScopeParamInfo::float64(drv.p_noise_amplitude));
    m.insert("VertGain_RBV".into(), ScopeParamInfo::float64(drv.p_vert_gain));
    m.insert("TimePerDiv_RBV".into(), ScopeParamInfo::float64(drv.p_time_per_div));
    m.insert("VoltsPerDivSelect_RBV".into(), ScopeParamInfo::enumerated(drv.p_volts_per_div_select));
    m.insert("VoltsPerDiv_RBV".into(), ScopeParamInfo::float64(drv.p_volts_per_div));
    m.insert("MinValue_RBV".into(), ScopeParamInfo::float64(drv.p_min_value));
    m.insert("MaxValue_RBV".into(), ScopeParamInfo::float64(drv.p_max_value));
    m.insert("MeanValue_RBV".into(), ScopeParamInfo::float64(drv.p_mean_value));

    // Waveform records
    m.insert("Waveform_RBV".into(), ScopeParamInfo::float64_array(drv.p_waveform));
    m.insert("TimeBase_RBV".into(), ScopeParamInfo::float64_array(drv.p_time_base));

    m
}

struct ScopeDeviceSupport {
    driver: Arc<Mutex<ScopeSimulator>>,
    registry: Arc<ScopeParamRegistry>,
    mapping: Option<ScopeParamInfo>,
    record_name: String,
    scan: ScanType,
    _interrupt_sub: Option<InterruptSubscription>,
}

impl ScopeDeviceSupport {
    fn new(driver: Arc<Mutex<ScopeSimulator>>, registry: Arc<ScopeParamRegistry>) -> Self {
        Self {
            driver, registry, mapping: None, record_name: String::new(),
            scan: ScanType::Passive, _interrupt_sub: None,
        }
    }
}

const ST_FIELDS: &[&str] = &[
    "ZRST", "ONST", "TWST", "THST", "FRST", "FVST", "SXST", "SVST",
    "EIST", "NIST", "TEST", "ELST", "TVST", "TTST", "FTST", "FFST",
];

/// Push driver enum choices to record's xxST fields.
fn push_enum_choices(record: &mut dyn Record, choices: &[asyn_rs::param::EnumEntry]) {
    for (i, field) in ST_FIELDS.iter().enumerate() {
        let s = choices.get(i).map(|e| e.string.clone()).unwrap_or_default();
        let _ = record.put_field(field, EpicsValue::String(s));
    }
}

impl DeviceSupport for ScopeDeviceSupport {
    fn dtyp(&self) -> &str { "asynScopeSimulator" }

    fn set_record_info(&mut self, name: &str, scan: ScanType) {
        self.record_name = name.to_string();
        self.scan = scan;
        let suffix = name.rsplit(':').next().unwrap_or(name);
        if let Some(info) = self.registry.get(suffix) {
            self.mapping = Some(*info);
        } else {
            eprintln!("asynScopeSimulator: no param mapping for suffix '{suffix}' (record: {name})");
        }
    }

    fn init(&mut self, record: &mut dyn Record) -> CaResult<()> {
        let info = match self.mapping {
            Some(info) => info,
            None => return Ok(()),
        };
        // Push initial enum choices at init time (before PINI, before clients connect)
        if matches!(info.param_type, ScopeParamType::Enum) {
            let drv = self.driver.lock();
            if let Ok((_, choices)) = drv.base.params.get_enum(info.param_index, 0) {
                push_enum_choices(record, &choices);
            }
        }
        Ok(())
    }

    fn io_intr_receiver(&mut self) -> Option<epics_base_rs::runtime::sync::mpsc::Receiver<()>> {
        if self.scan != ScanType::IoIntr {
            return None;
        }
        let info = self.mapping?;
        let filter = InterruptFilter {
            reason: Some(info.param_index),
            addr: Some(0),
        };
        let (sub, mut intr_rx) = {
            let drv = self.driver.lock();
            drv.base.interrupts.register_interrupt_user(filter)
        };
        self._interrupt_sub = Some(sub);
        let (tx, rx) = epics_base_rs::runtime::sync::mpsc::channel(16);
        epics_base_rs::runtime::task::spawn(async move {
            while intr_rx.recv().await.is_some() {
                if tx.send(()).await.is_err() {
                    break;
                }
            }
        });
        Some(rx)
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let info = match self.mapping {
            Some(info) => info,
            None => return Ok(DeviceReadOutcome::ok()),
        };
        let drv = self.driver.lock();
        match info.param_type {
            ScopeParamType::Enum => {
                if let Ok((idx, choices)) = drv.base.params.get_enum(info.param_index, 0) {
                    record.set_val(EpicsValue::Enum(idx as u16))?;
                    push_enum_choices(record, &choices);
                } else {
                    let val = drv.base.params.get_int32(info.param_index, 0)
                        .map_err(|e| CaError::InvalidValue(e.to_string()))?;
                    record.set_val(EpicsValue::Enum(val as u16))?;
                }
            }
            ScopeParamType::Int32 => {
                let val = drv.base.params.get_int32(info.param_index, 0)
                    .map_err(|e| CaError::InvalidValue(e.to_string()))?;
                record.set_val(EpicsValue::Long(val))?;
            }
            ScopeParamType::Float64 => {
                let val = drv.base.params.get_float64(info.param_index, 0)
                    .map_err(|e| CaError::InvalidValue(e.to_string()))?;
                record.set_val(EpicsValue::Double(val))?;
            }
            ScopeParamType::Float64Array => {
                let arr = drv.base.params.get_float64_array(info.param_index, 0)
                    .unwrap_or_default();
                record.set_val(EpicsValue::DoubleArray(arr.to_vec()))?;
            }
        }
        Ok(DeviceReadOutcome::ok())
    }

    fn write(&mut self, record: &mut dyn Record) -> CaResult<()> {
        let info = match self.mapping {
            Some(info) => info,
            None => return Ok(()),
        };
        let val = record.val()
            .ok_or_else(|| CaError::InvalidValue("no VAL".into()))?;

        let mut drv = self.driver.lock();
        match info.param_type {
            ScopeParamType::Enum => {
                let v = val.to_f64()
                    .ok_or_else(|| CaError::InvalidValue("cannot convert to enum".into()))? as i32;
                let mut user = AsynUser::new(info.param_index);
                drv.write_int32(&mut user, v)
                    .map_err(|e| CaError::InvalidValue(e.to_string()))?;
            }
            ScopeParamType::Int32 => {
                let v = val.to_f64()
                    .ok_or_else(|| CaError::InvalidValue("cannot convert to i32".into()))? as i32;
                let mut user = AsynUser::new(info.param_index);
                drv.write_int32(&mut user, v)
                    .map_err(|e| CaError::InvalidValue(e.to_string()))?;
            }
            ScopeParamType::Float64 => {
                let v = val.to_f64()
                    .ok_or_else(|| CaError::InvalidValue("cannot convert to f64".into()))?;
                let mut user = AsynUser::new(info.param_index);
                drv.write_float64(&mut user, v)
                    .map_err(|e| CaError::InvalidValue(e.to_string()))?;
            }
            ScopeParamType::Float64Array => {}
        }
        Ok(())
    }
}

// ========== DriverHolder + Command Handlers ==========

struct DriverHolder {
    driver: std::sync::Mutex<Option<Arc<Mutex<ScopeSimulator>>>>,
    registry: std::sync::Mutex<Option<Arc<ScopeParamRegistry>>>,
}

impl DriverHolder {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            driver: std::sync::Mutex::new(None),
            registry: std::sync::Mutex::new(None),
        })
    }
}

struct ConfigHandler {
    holder: Arc<DriverHolder>,
    handle: epics_base_rs::runtime::task::RuntimeHandle,
}

impl CommandHandler for ConfigHandler {
    fn call(&self, args: &[ArgValue], _ctx: &CommandContext) -> CommandResult {
        let port_name = match &args[0] {
            ArgValue::String(s) => s.clone(),
            _ => return Err("portName required".into()),
        };

        println!("scopeSimulatorConfig: port={port_name}");

        let notify = Arc::new(Notify::new());
        let driver = ScopeSimulator::new(&port_name, notify.clone());
        let registry = Arc::new(build_param_registry(&driver));
        let indices = driver.param_indices();
        let port = Arc::new(Mutex::new(driver));

        let sim_port = port.clone();
        self.handle.spawn(async move {
            sim_task(sim_port, notify, indices).await;
        });

        *self.holder.driver.lock().unwrap() = Some(port);
        *self.holder.registry.lock().unwrap() = Some(registry);

        Ok(CommandOutcome::Continue)
    }
}

struct ReportHandler {
    holder: Arc<DriverHolder>,
}

impl CommandHandler for ReportHandler {
    fn call(&self, _args: &[ArgValue], _ctx: &CommandContext) -> CommandResult {
        let guard = self.holder.driver.lock().unwrap();
        let driver = match guard.as_ref() {
            Some(d) => d,
            None => {
                println!("No ScopeSimulator configured");
                return Ok(CommandOutcome::Continue);
            }
        };

        let d = driver.lock();
        let base = &d.base;

        let run = base.params.get_int32(d.p_run, 0).unwrap_or(0);
        let max_pts = base.params.get_int32(d.p_max_points, 0).unwrap_or(0);
        let update_t = base.params.get_float64(d.p_update_time, 0).unwrap_or(0.0);
        let vert_gain = base.params.get_float64(d.p_vert_gain, 0).unwrap_or(0.0);
        let vpd = base.params.get_float64(d.p_volts_per_div, 0).unwrap_or(0.0);
        let tpd = base.params.get_float64(d.p_time_per_div, 0).unwrap_or(0.0);
        let noise = base.params.get_float64(d.p_noise_amplitude, 0).unwrap_or(0.0);
        let offset = base.params.get_float64(d.p_volt_offset, 0).unwrap_or(0.0);
        let min_v = base.params.get_float64(d.p_min_value, 0).unwrap_or(0.0);
        let max_v = base.params.get_float64(d.p_max_value, 0).unwrap_or(0.0);
        let mean_v = base.params.get_float64(d.p_mean_value, 0).unwrap_or(0.0);

        println!("ScopeSimulator Report");
        println!("  Run:            {}", if run != 0 { "Yes" } else { "No" });
        println!("  MaxPoints:      {max_pts}");
        println!("  UpdateTime:     {update_t:.3} s");
        println!("  VertGain:       {vert_gain:.0}x");
        println!("  VoltsPerDiv:    {vpd:.3} V");
        println!("  TimePerDiv:     {tpd:.4} s");
        println!("  VoltOffset:     {offset:.3} V");
        println!("  NoiseAmplitude: {noise:.3} V");
        println!("  MinValue:       {min_v:.4}");
        println!("  MaxValue:       {max_v:.4}");
        println!("  MeanValue:      {mean_v:.4}");

        Ok(CommandOutcome::Continue)
    }
}

// ========== Main ==========

#[epics_base_rs::epics_main]
async fn main() -> CaResult<()> {
    let args: Vec<String> = std::env::args().collect();

    epics_base_rs::runtime::env::set_default("SCOPE_IOC", env!("CARGO_MANIFEST_DIR"));

    let script = if args.len() > 1 && !args[1].starts_with('-') {
        args[1].clone()
    } else {
        eprintln!("Usage: scope_ioc <st.cmd>");
        std::process::exit(1);
    };

    let holder = DriverHolder::new();
    let holder_for_config = holder.clone();
    let holder_for_factory = holder.clone();
    let holder_for_report = holder.clone();
    let handle = epics_base_rs::runtime::task::runtime_handle();

    IocApplication::new()
        .port(
            std::env::var("EPICS_CA_SERVER_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5064),
        )
        .register_startup_command(CommandDef::new(
            "scopeSimulatorConfig",
            vec![ArgDesc { name: "portName", arg_type: ArgType::String, optional: false }],
            "scopeSimulatorConfig portName - Configure scope simulator driver",
            ConfigHandler { holder: holder_for_config, handle },
        ))
        .register_device_support("asynScopeSimulator", move || {
            let driver = holder_for_factory.driver.lock().unwrap()
                .as_ref()
                .expect("scopeSimulatorConfig must be called before iocInit")
                .clone();
            let registry = holder_for_factory.registry.lock().unwrap()
                .as_ref()
                .expect("scopeSimulatorConfig must be called before iocInit")
                .clone();
            Box::new(ScopeDeviceSupport::new(driver, registry))
        })
        .register_shell_command(CommandDef::new(
            "scopeSimulatorReport",
            vec![ArgDesc { name: "level", arg_type: ArgType::Int, optional: true }],
            "scopeSimulatorReport [level] - Report scope simulator status",
            ReportHandler { holder: holder_for_report },
        ))
        .startup_script(&script)
        .run(epics_ca_rs::server::run_ca_ioc)
        .await
}
