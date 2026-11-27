use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::InterfaceType;
use asyn_rs::interrupt::{AsyncInterruptReceiver, AsyncRecvError, InterruptFilter};
use asyn_rs::param::ParamValue;
use asyn_rs::port::DrvUserRequest;
use asyn_rs::port_handle::PortHandle;
use asyn_rs::sync_io::SyncIOHandle;
use asyn_rs::trace::TraceMask;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{
    DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, DeviceUdf,
};
use epics_base_rs::server::recgbl::alarm_status::LINK_ALARM;
use epics_base_rs::server::record::{AlarmSeverity, CommonFields, Record, ScanType};
use epics_libcom_rs::epics_println;
use epics_libcom_rs::runtime::stdlib::atoi;
use epics_libcom_rs::runtime::task::{
    StackSizeClass, ThreadPriority, block_on_sync, spawn_dedicated_thread,
};

use crate::records::epid::EpidRecord;

/// Samples the input port's data interrupts may queue ahead of the loop; a
/// full ring drops its oldest.
const DATA_RING: usize = 1024;
/// Interval updates queued the same way.
const INTERVAL_RING: usize = 16;
/// How long an output write or bumpless read may wait for the output port.
const OUTPUT_TIMEOUT: Duration = Duration::from_secs(1);

/// Fast Epid device support using asyn driver for high-speed (1+ kHz) PID.
///
/// Ported from `devEpidFast.c`. The PID computation runs on a dedicated
/// thread driven by asyn interrupt callbacks, not during record
/// processing. The record merely copies parameters to/from the fast
/// computation thread.
///
/// # Architecture
///
/// ```text
/// ┌─────────────┐    interrupt     ┌──────────────────┐
/// │ asyn driver  │ ──────────────► │ PID input thread  │
/// │ (input ADC)  │    (new cval)   │   (epidFast)      │
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
/// A record loaded with `DTYP "Fast Epid"` connects itself at init from its
/// `INP` and `DESC`; the `start_*` and [`set_output_port`](Self::set_output_port)
/// entries wire the same loop by hand.
pub struct EpidFastDeviceSupport {
    pvt: Arc<Mutex<EpidFastPvt>>,
    record_name: String,
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

    // Output port writer (set by `connect`)
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
            record_name: String::new(),
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
    pub fn set_output_port(&self, sync_io: SyncIOHandle, reason: usize) {
        self.connect(
            OutputLink::Port {
                io: sync_io,
                reason,
            },
            None,
        )
        .expect("an output alone starts no thread");
    }

    /// Start the interrupt-driven PID callback loop.
    ///
    /// Starts the `epidFast` thread, which receives new readback values
    /// from `input_rx` and runs `do_pid()` on each. This is the high-speed
    /// PID path that runs at the interrupt rate (1kHz+), independent of
    /// record processing.
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
        input_rx: tokio::sync::mpsc::Receiver<f64>,
        output_fn: Arc<Mutex<dyn FnMut(f64) + Send>>,
    ) {
        self.connect(
            OutputLink::Writer(output_fn),
            Some(InputLink::Samples(input_rx)),
        )
        .expect("cannot start the epidFast thread");
    }

    /// Start the PID callback loop driven by an asyn Float64 output port.
    ///
    /// Identical to [`start_callback_loop`] but takes the output asyn
    /// port handle directly and installs *both* the output writer and
    /// the bumpless-transfer output reader from it, as
    /// [`set_output_port`] does — mirroring C `devEpidFast.c` `init_record`,
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
        output_sync_io: SyncIOHandle,
        output_reason: usize,
    ) {
        self.connect(
            OutputLink::Port {
                io: output_sync_io,
                reason: output_reason,
            },
            Some(InputLink::Samples(input_rx)),
        )
        .expect("cannot start the epidFast thread");
    }

    /// Start from an asyn interrupt subscription.
    ///
    /// Feeds the subscription's `input_reason` Float64 interrupts into the
    /// PID callback loop.
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
        interrupt_rx: AsyncInterruptReceiver,
        input_reason: usize,
        output_fn: Arc<Mutex<dyn FnMut(f64) + Send>>,
    ) {
        self.connect(
            OutputLink::Writer(output_fn),
            Some(InputLink::Interrupts {
                data: interrupt_rx,
                reason: input_reason,
                interval: None,
            }),
        )
        .expect("cannot start the epidFast thread");
    }

    /// Start from an asyn interrupt subscription, driven by an asyn
    /// Float64 output port.
    ///
    /// Identical to [`start_from_asyn_interrupts`] but takes the output
    /// asyn port handle directly and installs *both* the output writer
    /// and the bumpless-transfer output reader from it, as
    /// [`set_output_port`] does — mirroring C `devEpidFast.c` `init_record`,
    /// which captures a single `pPvt->pfloat64Output` interface and uses
    /// it for both `read` (devEpidFast.c:446-448) and `write`
    /// (devEpidFast.c:471-473).
    ///
    /// [`start_from_asyn_interrupts`]: Self::start_from_asyn_interrupts
    /// [`set_output_port`]: Self::set_output_port
    pub fn start_from_asyn_interrupts_with_port(
        &self,
        interrupt_rx: AsyncInterruptReceiver,
        input_reason: usize,
        output_sync_io: SyncIOHandle,
        output_reason: usize,
    ) {
        self.connect(
            OutputLink::Port {
                io: output_sync_io,
                reason: output_reason,
            },
            Some(InputLink::Interrupts {
                data: interrupt_rx,
                reason: input_reason,
                interval: None,
            }),
        )
        .expect("cannot start the epidFast thread");
    }

    /// The one place the loop is wired, for a loaded record and the
    /// hand-wired entries alike: install `output`, then start the
    /// `epidFast` thread on `input`.
    fn connect(&self, output: OutputLink, input: Option<InputLink>) -> std::io::Result<()> {
        type Writer = Arc<Mutex<dyn FnMut(f64) + Send>>;
        type Reader = Arc<Mutex<dyn FnMut() -> Option<f64> + Send>>;
        let (writer, reader): (Writer, Option<Reader>) = match output {
            OutputLink::Port { io, reason } => {
                let writer_io = Arc::new(io);
                let reader_io = Arc::clone(&writer_io);
                let writer: Writer = Arc::new(Mutex::new(move |v: f64| {
                    // C `do_PID` (devEpidFast.c:471-478).
                    if let Err(e) = writer_io.write_float64(reason, v) {
                        writer_io.print(
                            reason as i32,
                            TraceMask::ERROR,
                            format_args!("devEpidFast, error writing output {e}"),
                        );
                    }
                }));
                let reader: Reader =
                    Arc::new(Mutex::new(move || reader_io.read_float64(reason).ok()));
                (writer, Some(reader))
            }
            OutputLink::Writer(writer) => (writer, None),
        };
        {
            let mut p = self.pvt.lock().unwrap();
            p.output_writer = Some(writer);
            if reader.is_some() {
                p.output_reader = reader;
            }
        }
        let Some(input) = input else {
            return Ok(());
        };
        let pvt = Arc::clone(&self.pvt);
        spawn_dedicated_thread(
            "epidFast".into(),
            ThreadPriority::High,
            StackSizeClass::Medium,
            move || {
                if let Err(e) = block_on_sync(run_input(pvt, input)) {
                    epics_println!("devEpidFast: the epidFast thread cannot block: {e:?}");
                }
            },
        )?;
        Ok(())
    }

    /// C `init_record`'s connects (devEpidFast.c:186-303), in its order.
    fn connect_record(&self, setup: &FastEpidSetup) -> Result<(), String> {
        let name = &self.record_name;
        let prefix = format!("devEpidFast::init_record {name},");

        let input = connect_device(&setup.input_name, setup.input_channel).ok_or_else(|| {
            format!(
                "devEpidFast::init_record, {name} error in connectDevice to input \
                 asynManager:connectDevice port {} not found",
                setup.input_name
            )
        })?;
        find_interfaces(&input, &prefix)?;
        let data_reason =
            drv_user_create(&input, &setup.input_data, setup.input_channel).map_err(|e| {
                format!(
                    "{prefix} asynDrvUser->create failed for input data string {} {e}",
                    setup.input_data
                )
            })?;
        let data = subscribe(&input, data_reason, setup.input_channel, DATA_RING).map_err(|e| {
            format!("{prefix} pfloat64Input->registerInterruptUser failed for dataCallback {e}")
        })?;

        input.connect_device(0);
        let interval_reason = drv_user_create(&input, &setup.input_interval, 0).map_err(|e| {
            format!(
                "{prefix} asynDrvUser->create failed for interval string {} {e}",
                setup.input_interval
            )
        })?;
        let interval = subscribe(&input, interval_reason, 0, INTERVAL_RING).map_err(|e| {
            format!("{prefix} pfloat64Input->registerInterruptUser failed for intervalCallback {e}")
        })?;
        let callback_interval = input
            .read_float64_blocking(interval_reason, 0)
            .map_err(|e| format!("{prefix} pfloat64Input->read failed for callbackInterval {e}"))?;
        self.pvt.lock().unwrap().callback_interval = callback_interval;

        let output = connect_device(&setup.output_name, setup.output_channel).ok_or_else(|| {
            format!(
                "{prefix} error in connectDevice to output \
                     asynManager:connectDevice port {} not found",
                setup.output_name
            )
        })?;
        find_interfaces(&output, &prefix)?;
        let output_reason = drv_user_create(&output, &setup.output_data, setup.output_channel)
            .map_err(|e| {
                format!(
                    "{prefix} asynDrvUser->create failed for output data string {} {e}",
                    setup.output_data
                )
            })?;
        let io = SyncIOHandle::from_handle(
            output,
            setup.output_channel,
            OUTPUT_TIMEOUT,
            InterfaceType::Float64,
        )
        .map_err(|e| format!("{prefix} cannot find asynFloat64 interface {e}"))?;

        self.connect(
            OutputLink::Port {
                io,
                reason: output_reason,
            },
            Some(InputLink::Interrupts {
                data,
                reason: data_reason,
                interval: Some(interval),
            }),
        )
        .map_err(|e| format!("{prefix} cannot start the epidFast thread: {e}"))
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

/// Where `do_pid` sends its output.
enum OutputLink {
    /// C `pfloat64Output`: written each cycle and read on the bumpless
    /// OFF->ON edge.
    Port { io: SyncIOHandle, reason: usize },
    /// A caller's closure; the bumpless reader is left as it was.
    Writer(Arc<Mutex<dyn FnMut(f64) + Send>>),
}

/// What drives `do_pid` on the `epidFast` thread.
enum InputLink {
    Samples(tokio::sync::mpsc::Receiver<f64>),
    /// C `dataCallback` on `reason`, and `intervalCallback` when the
    /// interval is subscribed.
    Interrupts {
        data: AsyncInterruptReceiver,
        reason: usize,
        interval: Option<AsyncInterruptReceiver>,
    },
}

async fn run_input(pvt: Arc<Mutex<EpidFastPvt>>, input: InputLink) {
    match input {
        InputLink::Samples(mut rx) => {
            while let Some(v) = rx.recv().await {
                pvt.lock().unwrap().do_pid(v);
            }
        }
        InputLink::Interrupts {
            mut data,
            reason,
            mut interval,
        } => loop {
            epics_libcom_rs::runtime::select! {
                got = data.recv() => match got {
                    Ok(iv) if iv.reason == reason => {
                        if let Some(v) = sample(&iv.value) {
                            pvt.lock().unwrap().do_pid(v);
                        }
                    }
                    Ok(_) | Err(AsyncRecvError::Lagged(_)) => {}
                    Err(AsyncRecvError::Closed) => break,
                },
                got = next_interval(&mut interval) => match got {
                    Ok(iv) => {
                        if let Some(seconds) = sample(&iv.value) {
                            pvt.lock().unwrap().interval_callback(seconds);
                        }
                    }
                    Err(AsyncRecvError::Lagged(_)) => {}
                    Err(AsyncRecvError::Closed) => interval = None,
                },
            }
        },
    }
}

/// The next interval update, or never when there is no subscription.
async fn next_interval(
    interval: &mut Option<AsyncInterruptReceiver>,
) -> Result<asyn_rs::interrupt::InterruptValue, AsyncRecvError> {
    match interval {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

fn sample(value: &ParamValue) -> Option<f64> {
    match value {
        ParamValue::Float64(f) => Some(*f),
        ParamValue::Int32(i) => Some(*i as f64),
        ParamValue::Int64(i) => Some(*i as f64),
        _ => None,
    }
}

/// C `init_record`'s seven `epicsStrtok_r(temp, ", ")` tokens of
/// `INP + " " + DESC` (devEpidFast.c:126-182); tokens past the seventh are
/// ignored, as in C.
struct FastEpidSetup {
    input_name: String,
    input_channel: i32,
    input_data: String,
    input_interval: String,
    output_name: String,
    output_channel: i32,
    output_data: String,
}

impl FastEpidSetup {
    /// `Err` is C's name for the first missing token.
    fn parse(joined: &str) -> Result<Self, &'static str> {
        let mut tokens = joined.split([',', ' ']).filter(|t| !t.is_empty());
        let mut next = |missing: &'static str| tokens.next().ok_or(missing);
        Ok(Self {
            input_name: next("inputName")?.to_owned(),
            input_channel: atoi(next("inputChannel")?),
            input_data: next("inputDataString")?.to_owned(),
            input_interval: next("inputIntervalString")?.to_owned(),
            output_name: next("outputName")?.to_owned(),
            output_channel: atoi(next("outputChannel")?),
            output_data: next("outputDataString")?.to_owned(),
        })
    }
}

/// C `pasynManager->connectDevice`: `None` when no port has that name.
fn connect_device(port: &str, addr: i32) -> Option<PortHandle> {
    let handle = asyn_rs::registry::get_port(port)?.handle;
    handle.connect_device(addr);
    Some(handle)
}

/// C's `findInterface` of asynFloat64 then asynDrvUser.
fn find_interfaces(handle: &PortHandle, prefix: &str) -> Result<(), String> {
    for (iface, c_name) in [
        (InterfaceType::Float64, "asynFloat64"),
        (InterfaceType::DrvUser, "asynDrvUser"),
    ] {
        if !handle.has_interface(iface) {
            return Err(format!(
                "{prefix} cannot find {c_name} interface on port {}",
                handle.port_name()
            ));
        }
    }
    Ok(())
}

fn drv_user_create(handle: &PortHandle, drv_info: &str, addr: i32) -> AsynResult<usize> {
    let req = DrvUserRequest::new(drv_info, addr).with_iface(InterfaceType::Float64);
    handle
        .drv_user_create_blocking(&req)
        .map(|info| info.reason)
}

/// C `pfloat64Input->registerInterruptUser` on `reason` at `addr`.
fn subscribe(
    handle: &PortHandle,
    reason: usize,
    addr: i32,
    ring: usize,
) -> AsynResult<AsyncInterruptReceiver> {
    let filter = InterruptFilter {
        reason: Some(reason),
        addr: Some(addr),
        iface: Some(InterfaceType::Float64),
        ..Default::default()
    };
    handle
        .interrupts()
        .subscribe_async_filtered(filter, ring, 0)
}

impl DeviceSupport for EpidFastDeviceSupport {
    fn dtyp(&self) -> &str {
        "Fast Epid"
    }

    fn set_record_info(&mut self, name: &str, _scan: ScanType) {
        self.record_name = name.to_owned();
    }

    /// C `init_record` (devEpidFast.c:103-309).
    fn init_common(
        &mut self,
        record: &mut dyn Record,
        common: &mut CommonFields,
    ) -> CaResult<DeviceInitOutcome> {
        let inp = common.text.inp.as_str_lossy();
        let inp = inp.trim_start();
        let inp = inp.strip_prefix('@').unwrap_or(inp);
        let joined = format!("{inp} {}", common.text.desc.as_str_lossy());
        let wired = FastEpidSetup::parse(&joined)
            .map_err(|missing| {
                format!(
                    "devEpidFast::init_record {}, INP field=\"{joined}\" is missing {missing}",
                    self.record_name
                )
            })
            .and_then(|setup| self.connect_record(&setup));
        if let Err(message) = wired {
            epics_println!("{message}");
            return Ok(DeviceInitOutcome::dead_with_alarm(
                LINK_ALARM,
                AlarmSeverity::Invalid,
            ));
        }
        let epid = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<EpidRecord>())
            .expect("EpidFastDeviceSupport requires an EpidRecord");
        // C `update_params` (devEpidFast.c:304), which sets `udf = 0`.
        self.update_params_from_record(epid);
        self.update_record_from_params(epid);
        Ok(DeviceInitOutcome::Ok(DeviceUdf::Defined))
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
