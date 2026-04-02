use std::sync::{Arc, Mutex};
use std::time::Instant;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport};
use epics_base_rs::server::record::Record;

use crate::records::epid::EpidRecord;

/// Fast Epid device support using asyn driver for high-speed (1+ kHz) PID.
///
/// Ported from `devEpidFast.c`. The PID computation runs in a background
/// tokio task driven by asyn interrupt callbacks, not during record
/// processing. The record merely copies parameters to/from the fast
/// computation thread.
///
/// # Architecture
///
/// ```text
/// ┌─────────────┐    interrupt     ┌──────────────────┐
/// │ asyn driver  │ ──────────────► │ PID callback task │
/// │ (input ADC)  │    (new cval)   │  (tokio::spawn)   │
/// └─────────────┘                  │  runs do_pid()    │
///                                  │  writes output    │
///       ┌──────────────────────────┤  to output driver │
///       │  shared EpidFastPvt      └──────────────────┘
///       │  (Arc<Mutex>)                    ▲
///       ▼                                  │
/// ┌─────────────┐  read()         params   │
/// │ EpidRecord   │ ◄─────── copy ──────────┘
/// │ (process)    │ ────────► copy ──────────►
/// └─────────────┘  results
/// ```
///
/// The `start_callback_loop()` method spawns the background task.
/// Call it after connecting to the asyn input port.
pub struct EpidFastDeviceSupport {
    pvt: Arc<Mutex<EpidFastPvt>>,
}

/// Private state for the fast PID loop, shared between the
/// record process thread and the interrupt callback task.
pub struct EpidFastPvt {
    // PID parameters (copied from record on each process cycle)
    pub kp: f64,
    pub ki: f64,
    pub kd: f64,
    pub drvh: f64,
    pub drvl: f64,
    pub val: f64, // setpoint
    pub fbon: bool,
    pub fmod: i16,

    // PID state (updated by callback, read by record process)
    pub cval: f64,
    pub oval: f64,
    pub err: f64,
    pub p: f64,
    pub i: f64,
    pub d: f64,
    pub dt: f64,
    pub ct: Instant,
    pub fbop: bool,

    // Averaging
    pub num_average: u32,
    pub accumulated: f64,
    pub count: u32,

    // Output port writer (set by start_callback_loop)
    pub output_writer: Option<Arc<Mutex<dyn FnMut(f64) + Send>>>,
}

impl Default for EpidFastPvt {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            kp: 0.0,
            ki: 0.0,
            kd: 0.0,
            drvh: 0.0,
            drvl: 0.0,
            val: 0.0,
            fbon: false,
            fmod: 0,
            cval: 0.0,
            oval: 0.0,
            err: 0.0,
            p: 0.0,
            i: 0.0,
            d: 0.0,
            dt: 0.0,
            ct: now,
            fbop: false,
            num_average: 1,
            accumulated: 0.0,
            count: 0,
            output_writer: None,
        }
    }
}

impl EpidFastPvt {
    /// Execute one PID cycle on new data. Called from the interrupt callback task.
    /// After computing the output, writes to the output port if configured.
    pub fn do_pid(&mut self, new_cval: f64) {
        // Averaging
        self.accumulated += new_cval;
        self.count += 1;
        if self.count < self.num_average {
            return;
        }
        let cval = self.accumulated / self.count as f64;
        self.accumulated = 0.0;
        self.count = 0;

        let pcval = self.cval;
        self.cval = cval;

        let ct = Instant::now();
        let dt = ct.duration_since(self.ct).as_secs_f64();
        self.ct = ct;
        self.dt = dt;

        let ep = self.err;
        let mut oval = self.oval;

        match self.fmod {
            0 => {
                // PID mode
                let e = self.val - cval;
                let de = e - ep;
                self.p = self.kp * e;
                let di = self.kp * self.ki * e * dt;

                if self.fbon {
                    if !self.fbop {
                        self.i = self.oval;
                    } else {
                        if (oval > self.drvl && oval < self.drvh)
                            || (oval >= self.drvh && di < 0.0)
                            || (oval <= self.drvl && di > 0.0)
                        {
                            self.i += di;
                            self.i = self.i.clamp(self.drvl, self.drvh);
                        }
                    }
                }
                if self.ki == 0.0 {
                    self.i = 0.0;
                }
                self.d = if dt > 0.0 {
                    self.kp * self.kd * (de / dt)
                } else {
                    0.0
                };
                self.err = e;
                oval = self.p + self.i + self.d;
            }
            1 => {
                // MaxMin mode
                if self.fbon {
                    if !self.fbop {
                        oval = self.oval;
                    } else {
                        let e = cval - pcval;
                        let sign = if self.d > 0.0 { 1.0 } else { -1.0 };
                        let sign = if (self.kp > 0.0 && e < 0.0) || (self.kp < 0.0 && e > 0.0) {
                            -sign
                        } else {
                            sign
                        };
                        self.d = self.kp * sign;
                        oval = self.oval + self.d;
                    }
                }
            }
            _ => {}
        }

        // Clamp output
        oval = oval.clamp(self.drvl, self.drvh);
        self.oval = oval;
        self.fbop = self.fbon;

        // Write output to hardware if configured
        if self.fbon {
            if let Some(ref writer) = self.output_writer {
                if let Ok(mut w) = writer.lock() {
                    w(self.oval);
                }
            }
        }
    }
}

impl Default for EpidFastDeviceSupport {
    fn default() -> Self {
        Self::new()
    }
}

impl EpidFastDeviceSupport {
    pub fn new() -> Self {
        Self {
            pvt: Arc::new(Mutex::new(EpidFastPvt::default())),
        }
    }

    /// Get a handle to the shared PID state for callback registration.
    pub fn pvt(&self) -> Arc<Mutex<EpidFastPvt>> {
        Arc::clone(&self.pvt)
    }

    /// Start the interrupt-driven PID callback loop.
    ///
    /// Spawns a tokio task that receives new readback values from `input_rx`
    /// and runs `do_pid()` on each. This is the high-speed PID path that
    /// runs at the interrupt rate (1kHz+), independent of record processing.
    ///
    /// `input_rx`: receives new controlled-variable values from the input driver
    /// `output_fn`: called with each new output value (writes to output driver)
    pub fn start_callback_loop(
        &self,
        mut input_rx: tokio::sync::mpsc::Receiver<f64>,
        output_fn: Arc<Mutex<dyn FnMut(f64) + Send>>,
    ) {
        let pvt = Arc::clone(&self.pvt);

        // Store the output writer in pvt
        {
            let mut p = pvt.lock().unwrap();
            p.output_writer = Some(output_fn);
        }

        tokio::spawn(async move {
            while let Some(new_cval) = input_rx.recv().await {
                let mut p = pvt.lock().unwrap();
                p.do_pid(new_cval);
            }
        });
    }

    /// Start from an asyn interrupt subscription.
    ///
    /// Subscribes to Float64 interrupts from the given broadcast sender
    /// and feeds them into the PID callback loop.
    pub fn start_from_asyn_interrupts(
        &self,
        mut interrupt_rx: tokio::sync::broadcast::Receiver<asyn_rs::interrupt::InterruptValue>,
        input_reason: usize,
        output_fn: Arc<Mutex<dyn FnMut(f64) + Send>>,
    ) {
        let pvt = Arc::clone(&self.pvt);

        {
            let mut p = pvt.lock().unwrap();
            p.output_writer = Some(output_fn);
        }

        tokio::spawn(async move {
            loop {
                match interrupt_rx.recv().await {
                    Ok(iv) => {
                        if iv.reason == input_reason {
                            let v = match &iv.value {
                                asyn_rs::param::ParamValue::Float64(f) => Some(*f),
                                asyn_rs::param::ParamValue::Int32(i) => Some(*i as f64),
                                asyn_rs::param::ParamValue::Int64(i) => Some(*i as f64),
                                _ => None,
                            };
                            if let Some(v) = v {
                                let mut p = pvt.lock().unwrap();
                                p.do_pid(v);
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Dropped some interrupts — continue
                    }
                }
            }
        });
    }

    /// Copy parameters from record to fast PID state.
    fn update_params_from_record(&self, epid: &EpidRecord) {
        let mut pvt = self.pvt.lock().unwrap();
        pvt.kp = epid.kp;
        pvt.ki = epid.ki;
        pvt.kd = epid.kd;
        pvt.drvh = epid.drvh;
        pvt.drvl = epid.drvl;
        pvt.val = epid.val;
        pvt.fbon = epid.fbon != 0;
        pvt.fmod = epid.fmod;
    }

    /// Copy computed results from fast PID state back to record.
    fn update_record_from_params(&self, epid: &mut EpidRecord) {
        let pvt = self.pvt.lock().unwrap();
        epid.cval = pvt.cval;
        epid.oval = pvt.oval;
        epid.err = pvt.err;
        epid.p = pvt.p;
        epid.i = pvt.i;
        epid.d = pvt.d;
        epid.dt = pvt.dt;
        epid.fbop = if pvt.fbop { 1 } else { 0 };
    }
}

impl DeviceSupport for EpidFastDeviceSupport {
    fn dtyp(&self) -> &str {
        "Fast Epid"
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let epid = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<EpidRecord>())
            .expect("EpidFastDeviceSupport requires an EpidRecord");

        // Copy parameters to fast PID (so callback loop uses latest gains)
        self.update_params_from_record(epid);
        // Copy latest results back to record (for display/alarm)
        self.update_record_from_params(epid);
        Ok(DeviceReadOutcome::computed())
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}
