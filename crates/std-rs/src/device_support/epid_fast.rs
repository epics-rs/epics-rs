use std::sync::{Arc, Mutex};
use std::time::Instant;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
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

    // Averaging — C `devEpidFast.c` `epidFastPvt`
    /// Interval (seconds) between successive driver data callbacks.
    /// C `callbackInterval`, set by `intervalCallback` from the driver
    /// and also used directly as `dt` in `do_PID` (devEpidFast.c:430).
    pub callback_interval: f64,
    /// Requested time-per-point (C `timePerPointRequested`), copied from
    /// the record's `DT` field by `update_params` (devEpidFast.c:320).
    pub time_per_point_requested: f64,
    /// Actual time-per-point achieved (C `timePerPointActual`) —
    /// `num_average * callback_interval`.
    pub time_per_point_actual: f64,
    pub num_average: u32,
    pub accumulated: f64,
    pub count: u32,

    // Output port writer (set by start_callback_loop)
    pub output_writer: Option<Arc<Mutex<dyn FnMut(f64) + Send>>>,

    /// Output port reader — C `devEpidFast.c:446-448` reads the actual
    /// current value of the output (DAC) on the feedback OFF->ON edge
    /// via `pPvt->pfloat64Output->read(...)` so the integral term is
    /// seeded bumplessly from the hardware's real output, not the
    /// last value the loop happened to command. When `None` (no
    /// output-port reader wired), the bumpless edge falls back to the
    /// last commanded `oval` — see `do_pid`.
    pub output_reader: Option<Arc<Mutex<dyn FnMut() -> Option<f64> + Send>>>,
}

impl Default for EpidFastPvt {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            // C `devEpidFast.c:121-123` init_record seeds KP=1 and
            // inverted drive limits (lowLimit=1, highLimit=-1) so that
            // before `update_params` runs the output clamp is a no-op.
            kp: 1.0,
            ki: 0.0,
            kd: 0.0,
            drvh: -1.0,
            drvl: 1.0,
            val: 0.0,
            fbon: false,
            cval: 0.0,
            oval: 0.0,
            err: 0.0,
            p: 0.0,
            i: 0.0,
            d: 0.0,
            dt: 0.0,
            ct: now,
            fbop: false,
            callback_interval: 0.0,
            time_per_point_requested: 0.0,
            time_per_point_actual: 0.0,
            num_average: 1,
            accumulated: 0.0,
            count: 0,
            output_writer: None,
            output_reader: None,
        }
    }
}

impl EpidFastPvt {
    /// Recompute the number of points to average and the resulting
    /// actual time-per-point. C `devEpidFast.c::computeNumAverage`
    /// (devEpidFast.c:356-362):
    /// `numAverage = 0.5 + timePerPointRequested/callbackInterval`,
    /// clamped to `>= 1`, then `timePerPointActual = numAverage *
    /// callbackInterval`.
    pub fn compute_num_average(&mut self) {
        let n = if self.callback_interval > 0.0 {
            (0.5 + self.time_per_point_requested / self.callback_interval) as i64
        } else {
            1
        };
        self.num_average = n.max(1) as u32;
        self.time_per_point_actual = self.num_average as f64 * self.callback_interval;
    }

    /// Callback from the driver when the data-callback interval changes.
    /// C `devEpidFast.c::intervalCallback` (devEpidFast.c:367-375):
    /// updates `callbackInterval` then recomputes `numAverage`.
    pub fn interval_callback(&mut self, seconds: f64) {
        self.callback_interval = seconds;
        self.compute_num_average();
    }

    /// Execute one PID cycle on new data. Called from the interrupt callback task.
    /// After computing the output, writes to the output port if configured.
    ///
    /// Mirrors C `devEpidFast.c::dataCallback` (averaging) +
    /// `do_PID` (devEpidFast.c:379-482).
    pub fn do_pid(&mut self, new_cval: f64) {
        // Averaging — C `dataCallback` (devEpidFast.c:379-398).
        // C shortcuts when numAverage == 1 (no need to accumulate).
        let cval = if self.num_average <= 1 {
            new_cval
        } else {
            self.accumulated += new_cval;
            self.count += 1;
            if self.count < self.num_average {
                return;
            }
            // C divides averageStore by `accumulated` (the running count),
            // which equals `count` here.
            let avg = self.accumulated / self.count as f64;
            self.accumulated = 0.0;
            self.count = 0;
            avg
        };

        self.cval = cval;

        // C `do_PID` (devEpidFast.c:430) uses `dt = pPvt->callbackInterval`
        // — the configured driver callback interval, NOT a measured
        // wall-clock difference. C devEpidFast keeps no `ct` timestamp.
        let dt = self.callback_interval;
        self.dt = self.time_per_point_actual;
        self.ct = Instant::now();

        let ep = self.err;
        let mut oval = self.oval;

        // C `do_PID` (devEpidFast.c:400-482) runs the PID algorithm
        // UNCONDITIONALLY — `epidFastPvt` has no `fmod` field and
        // `do_PID` never branches on FMOD. FMOD/MaxMin is honoured only
        // by the Soft device supports (`devEpidSoft.c:137`,
        // `devEpidSoftCallback.c:173` have `switch (pepid->fmod)`); the
        // Fast support ignores FMOD entirely.
        let e = self.val - cval;
        let de = e - ep;
        self.p = self.kp * e;
        let di = self.kp * self.ki * e * dt;

        if self.fbon {
            if !self.fbop {
                // Bumpless OFF->ON — C `devEpidFast.c:445-448`:
                //   pPvt->pfloat64Output->read(pPvt->float64OutputPvt,
                //       pPvt->pfloat64OutputAsynUser, &pPvt->I);
                // The integral term is seeded from the output port's
                // *actual current value* (the real DAC output) so the
                // loop turns on without a bump. When an `output_reader`
                // is wired, read it; otherwise fall back to the last
                // commanded `oval` (the closest available estimate).
                self.i = match &self.output_reader {
                    Some(reader) => reader
                        .lock()
                        .ok()
                        .and_then(|mut r| r())
                        .unwrap_or(self.oval),
                    None => self.oval,
                };
            } else if (oval > self.drvl && oval < self.drvh)
                || (oval >= self.drvh && di < 0.0)
                || (oval <= self.drvl && di > 0.0)
            {
                self.i += di;
                // C `devEpidFast.c:455-456` does two sequential
                // `if` clamps (low then high). `f64::clamp`
                // panics when min > max, which happens before
                // `update_params` seeds the limits — replicate
                // the panic-free sequential form.
                if self.i < self.drvl {
                    self.i = self.drvl;
                }
                if self.i > self.drvh {
                    self.i = self.drvh;
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

        // Clamp output — C `devEpidFast.c:464-465` sequential `if`
        // clamps (panic-free vs `f64::clamp` when drvl > drvh).
        if oval > self.drvh {
            oval = self.drvh;
        }
        if oval < self.drvl {
            oval = self.drvl;
        }
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

    /// Wire the output-port readback used for bumpless transfer.
    ///
    /// C `devEpidFast.c:446-448` reads the actual current output value
    /// (`pfloat64Output->read`) on the feedback OFF->ON edge to seed
    /// the integral term. Supply a closure that returns the output
    /// port's current value; without it the bumpless edge falls back
    /// to the last commanded `OVAL`.
    pub fn set_output_reader(&self, reader: Arc<Mutex<dyn FnMut() -> Option<f64> + Send>>) {
        if let Ok(mut p) = self.pvt.lock() {
            p.output_reader = Some(reader);
        }
    }

    /// Wire the asyn output port: install both the output writer and
    /// the bumpless-transfer output reader from a single asyn Float64
    /// port handle.
    ///
    /// This mirrors C `devEpidFast.c` exactly. `init_record`
    /// (devEpidFast.c:264-303) connects to *one* output asyn port and
    /// captures `pPvt->pfloat64Output` / `float64OutputPvt`. `do_PID`
    /// then uses that same interface for both directions:
    ///
    /// * the bumpless OFF->ON edge reads the output port's present
    ///   value — `pPvt->pfloat64Output->read(...)` (devEpidFast.c:446-448);
    /// * the feedback write — `pPvt->pfloat64Output->write(...)`
    ///   (devEpidFast.c:471-473).
    ///
    /// `sync_io` is the asyn Float64 output port handle and `reason`
    /// the parameter index (asyn `drvUser` "outputDataString" channel,
    /// devEpidFast.c:296-303). The reader calls `read_float64(reason)`
    /// on it; a failed read yields `None`, so `do_pid` falls back to
    /// the last commanded `OVAL` safety net.
    pub fn set_output_port(&self, sync_io: asyn_rs::sync_io::SyncIOHandle, reason: usize) {
        let sync_io = Arc::new(sync_io);
        let writer_io = Arc::clone(&sync_io);
        let reader_io = Arc::clone(&sync_io);

        let writer: Arc<Mutex<dyn FnMut(f64) + Send>> = Arc::new(Mutex::new(move |v: f64| {
            // C `devEpidFast.c:471-478` — write the output, log on error.
            let _ = writer_io.write_float64(reason, v);
        }));
        let reader: Arc<Mutex<dyn FnMut() -> Option<f64> + Send>> =
            Arc::new(Mutex::new(move || reader_io.read_float64(reason).ok()));

        if let Ok(mut p) = self.pvt.lock() {
            p.output_writer = Some(writer);
            p.output_reader = Some(reader);
        }
    }

    /// Start the interrupt-driven PID callback loop.
    ///
    /// Spawns a tokio task that receives new readback values from `input_rx`
    /// and runs `do_pid()` on each. This is the high-speed PID path that
    /// runs at the interrupt rate (1kHz+), independent of record processing.
    ///
    /// `input_rx`: receives new controlled-variable values from the input driver
    /// `output_fn`: called with each new output value (writes to output driver)
    ///
    /// This bare-closure form leaves the bumpless-transfer `output_reader`
    /// untouched — wire it separately with [`set_output_reader`] or
    /// [`set_output_port`], otherwise the OFF->ON edge falls back to the
    /// last commanded `OVAL`. When the output is a real asyn Float64
    /// port, prefer [`start_callback_loop_with_port`], which installs
    /// both writer and reader from the same port handle (mirroring C
    /// `devEpidFast.c` `pPvt->pfloat64Output`).
    ///
    /// [`set_output_reader`]: Self::set_output_reader
    /// [`set_output_port`]: Self::set_output_port
    /// [`start_callback_loop_with_port`]: Self::start_callback_loop_with_port
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

    /// Start the PID callback loop driven by an asyn Float64 output port.
    ///
    /// Identical to [`start_callback_loop`] but takes the output asyn
    /// port handle directly and installs *both* the output writer and
    /// the bumpless-transfer output reader from it via
    /// [`set_output_port`] — mirroring C `devEpidFast.c` `init_record`,
    /// which captures a single `pPvt->pfloat64Output` interface and uses
    /// it for both `read` (devEpidFast.c:446-448) and `write`
    /// (devEpidFast.c:471-473).
    ///
    /// `output_sync_io` is the asyn Float64 output port handle and
    /// `output_reason` the parameter index for the output channel.
    ///
    /// [`start_callback_loop`]: Self::start_callback_loop
    /// [`set_output_port`]: Self::set_output_port
    pub fn start_callback_loop_with_port(
        &self,
        input_rx: tokio::sync::mpsc::Receiver<f64>,
        output_sync_io: asyn_rs::sync_io::SyncIOHandle,
        output_reason: usize,
    ) {
        self.set_output_port(output_sync_io, output_reason);

        let pvt = Arc::clone(&self.pvt);
        let mut input_rx = input_rx;
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
    ///
    /// This bare-closure form leaves the bumpless-transfer `output_reader`
    /// untouched — wire it separately with [`set_output_reader`] or
    /// [`set_output_port`], or use [`start_from_asyn_interrupts_with_port`]
    /// to install both writer and reader from one asyn output port.
    ///
    /// [`set_output_reader`]: Self::set_output_reader
    /// [`set_output_port`]: Self::set_output_port
    /// [`start_from_asyn_interrupts_with_port`]: Self::start_from_asyn_interrupts_with_port
    pub fn start_from_asyn_interrupts(
        &self,
        interrupt_rx: tokio::sync::broadcast::Receiver<asyn_rs::interrupt::InterruptValue>,
        input_reason: usize,
        output_fn: Arc<Mutex<dyn FnMut(f64) + Send>>,
    ) {
        {
            let mut p = self.pvt.lock().unwrap();
            p.output_writer = Some(output_fn);
        }
        self.spawn_interrupt_loop(interrupt_rx, input_reason);
    }

    /// Start from an asyn interrupt subscription, driven by an asyn
    /// Float64 output port.
    ///
    /// Identical to [`start_from_asyn_interrupts`] but takes the output
    /// asyn port handle directly and installs *both* the output writer
    /// and the bumpless-transfer output reader from it via
    /// [`set_output_port`] — mirroring C `devEpidFast.c` `init_record`,
    /// which captures a single `pPvt->pfloat64Output` interface and uses
    /// it for both `read` (devEpidFast.c:446-448) and `write`
    /// (devEpidFast.c:471-473).
    ///
    /// [`start_from_asyn_interrupts`]: Self::start_from_asyn_interrupts
    /// [`set_output_port`]: Self::set_output_port
    pub fn start_from_asyn_interrupts_with_port(
        &self,
        interrupt_rx: tokio::sync::broadcast::Receiver<asyn_rs::interrupt::InterruptValue>,
        input_reason: usize,
        output_sync_io: asyn_rs::sync_io::SyncIOHandle,
        output_reason: usize,
    ) {
        self.set_output_port(output_sync_io, output_reason);
        self.spawn_interrupt_loop(interrupt_rx, input_reason);
    }

    /// Spawn the tokio task that feeds asyn Float64 interrupts into
    /// `do_pid`. Shared by both `start_from_asyn_interrupts` variants.
    fn spawn_interrupt_loop(
        &self,
        mut interrupt_rx: tokio::sync::broadcast::Receiver<asyn_rs::interrupt::InterruptValue>,
        input_reason: usize,
    ) {
        let pvt = Arc::clone(&self.pvt);
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
    ///
    /// C `devEpidFast.c::update_params` (devEpidFast.c:312-354): when the
    /// record's `DT` field (requested time-per-point) differs from the
    /// achieved `timePerPointActual`, the requested value is adopted and
    /// `numAverage` is recomputed.
    fn update_params_from_record(&self, epid: &EpidRecord) {
        let mut pvt = self.pvt.lock().unwrap();
        // C devEpidFast.c:319-322 — recompute averaging if DT changed.
        if epid.dt != pvt.time_per_point_actual {
            pvt.time_per_point_requested = epid.dt;
            pvt.compute_num_average();
        }
        pvt.kp = epid.kp;
        pvt.ki = epid.ki;
        pvt.kd = epid.kd;
        pvt.drvh = epid.drvh;
        pvt.drvl = epid.drvl;
        pvt.val = epid.val;
        pvt.fbon = epid.fbon != 0;
        // C `devEpidFast.c::update_params` (devEpidFast.c:332-339) copies
        // FBON/DRVH/DRVL/KP/KI/KD/VAL only — `epidFastPvt` has no `fmod`
        // field and the Fast support never reads FMOD.
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
        // C `update_params` writes `pepid->dt = pPvt->timePerPointActual`
        // unconditionally on every record process (devEpidFast.c:330), so
        // DT always reflects the current achieved time-per-point. Source
        // it from `time_per_point_actual` directly rather than from the
        // `dt` mirror, which `do_pid` updates only when a data callback
        // fires — otherwise DT would read 0 until the first callback.
        epid.dt = pvt.time_per_point_actual;
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
        // C `devEpidFast.c::read_epid:352-353` — `pepid->udf=0; return(0)`.
        // `computed` (not `converted`) because the port's status half also
        // carries "the PID already ran", which is what stops `process()`
        // running `do_pid` a second time; C expresses that by having no
        // record-side compute to skip at all.
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}
