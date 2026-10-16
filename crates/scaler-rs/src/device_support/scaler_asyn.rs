use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::interrupt_accept::InterruptGate;
use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::server::device_support::{DeviceInitOutcome, DeviceSupport};
use epics_base_rs::server::record::Record;

use crate::records::scaler::{MAX_SCALER_CHANNELS, ScalerRecord};

/// Asyn command strings for scaler drivers.
pub const SCALER_RESET_COMMAND: &str = "SCALER_RESET";
pub const SCALER_CHANNELS_COMMAND: &str = "SCALER_CHANNELS";
pub const SCALER_READ_COMMAND: &str = "SCALER_READ";
pub const SCALER_READ_SINGLE_COMMAND: &str = "SCALER_READ_SINGLE";
pub const SCALER_PRESET_COMMAND: &str = "SCALER_PRESET";
pub const SCALER_ARM_COMMAND: &str = "SCALER_ARM";
pub const SCALER_DONE_COMMAND: &str = "SCALER_DONE";

/// Trait for scaler hardware drivers.
pub trait ScalerDriver: Send + Sync + 'static {
    fn reset(&mut self) -> CaResult<()>;
    fn read(&mut self, counts: &mut [u32; MAX_SCALER_CHANNELS]) -> CaResult<()>;
    /// Program `preset` for `channel` and return the count the hardware
    /// will actually use.
    ///
    /// C parity: `scalerRecord.c:397-404` documents that some scalers
    /// (the Joerger VS64) set their own clock frequency and adjust the
    /// preset to match — "if device support has changed the preset, we
    /// recalc the preset from tp ... and call write_preset again". In C
    /// the dset `write_preset(scalerRecord *psr, ...)` mutates
    /// `psr->pr1` in place; the record then reads it back. The Rust
    /// trait makes that explicit by returning the programmed value.
    /// A driver that does not quantize simply returns `preset`
    /// unchanged (see `SoftScalerDriver`).
    fn write_preset(&mut self, channel: usize, preset: u32) -> CaResult<u32>;
    fn arm(&mut self, start: bool) -> CaResult<()>;
    /// The clock frequency (Hz) the driver is actually running at.
    ///
    /// C parity: `scalerRecord.c:399-403` — the VS64 "sets its own
    /// clock frequency"; `updateCounts` (`scalerRecord.c:585-587`) and
    /// the REQSTART reconciliation (`scalerRecord.c:424-430`) use the
    /// frequency the driver chose, not the one originally requested.
    /// A driver that does not own the clock returns `None`, leaving
    /// `FREQ` untouched.
    fn actual_frequency(&self) -> Option<f64> {
        None
    }
    /// C `registerInterruptUser` on `SCALER_DONE` (`devScalerAsyn.c:247-252`):
    /// the driver calls [`DoneInterrupt::callback`] with the done value
    /// whenever its C counterpart would make the int32 interrupt callback.
    /// An `asynPortDriver`-style driver holds a [`DoneParam`] and registers
    /// the user there.
    fn register_done_interrupt(&mut self, user: DoneInterrupt);
    fn num_channels(&self) -> usize;
}

type OnAccept = Arc<dyn Fn() + Send + Sync + 'static>;

/// C `interruptAccept` as a scaler record's done user sees it: the database's
/// gate once [`DeviceSupport::bind_interrupt_gate`] hands it over, closed
/// until then.
#[derive(Default)]
struct AcceptSlot {
    gate: OnceLock<Arc<InterruptGate>>,
    pending: Mutex<Vec<OnAccept>>,
}

impl AcceptSlot {
    fn is_open(&self) -> bool {
        self.gate.get().is_some_and(|g| g.is_open())
    }

    fn bind(&self, gate: &Arc<InterruptGate>) {
        let pending = {
            let mut pending = self.pending.lock().unwrap();
            if self.gate.set(Arc::clone(gate)).is_err() {
                return;
            }
            std::mem::take(&mut *pending)
        };
        for cb in pending {
            Self::arm(gate, cb);
        }
    }

    fn on_open(&self, cb: OnAccept) {
        let gate = {
            let mut pending = self.pending.lock().unwrap();
            match self.gate.get() {
                Some(gate) => Arc::clone(gate),
                None => return pending.push(cb),
            }
        };
        Self::arm(&gate, cb);
    }

    /// Run `cb` on every opening edge, and now if the gate is already open.
    fn arm(gate: &InterruptGate, cb: OnAccept) {
        let edge = Arc::clone(&cb);
        gate.on_open(move || edge());
        if gate.is_open() {
            cb();
        }
    }
}

/// One scaler record's done interrupt user: C `pPvt->done` plus the
/// `interruptCallback` that raises it (`devScalerAsyn.c:391-402`).
#[derive(Clone)]
pub struct DoneInterrupt {
    done: Arc<AtomicBool>,
    pulse: mpsc::Sender<()>,
    accept: Arc<AcceptSlot>,
}

impl DoneInterrupt {
    /// C `interruptCallback`: a 0 is ignored; otherwise set `done` and
    /// `callbackRequest` the record's `deviceCallbackFunc`, which processes
    /// it. A pulse already queued covers this one, as a pending
    /// `callbackRequest` does.
    pub fn callback(&self, value: i32) {
        if value == 0 {
            return;
        }
        self.done.store(true, Ordering::Release);
        let _ = self.pulse.try_send(());
    }

    /// C `interruptAccept` for this user's database.
    pub fn interrupt_accept(&self) -> bool {
        self.accept.is_open()
    }

    /// Run `cb` once `interruptAccept` goes true, and on each later opening.
    pub fn on_interrupt_accept(&self, cb: impl Fn() + Send + Sync + 'static) {
        self.accept.on_open(Arc::new(cb));
    }
}

struct DoneParamState {
    value: i32,
    changed: bool,
    users: Vec<DoneInterrupt>,
}

/// An `asynPortDriver` driver's `SCALER_DONE` int32 parameter and the
/// interrupt users registered on it.
///
/// `set` flags the parameter only when the value changes (C
/// `paramVal::setInteger`), and [`call_callbacks`](Self::call_callbacks)
/// delivers a flagged value to every user. Before `interruptAccept` it keeps
/// the flag (`asynPortDriver.cpp:838`), and the port's `callbackThread`
/// delivers it once the gate opens (`:923-937`): that is the boot replay that
/// processes the record of a driver constructed with `scalerDone = 1`.
#[derive(Clone)]
pub struct DoneParam(Arc<Mutex<DoneParamState>>);

impl DoneParam {
    /// The constructor's `setIntegerParam(scalerDone, initial)`: a newly
    /// defined parameter is flagged.
    pub fn new(initial: i32) -> Self {
        Self(Arc::new(Mutex::new(DoneParamState {
            value: initial,
            changed: true,
            users: Vec::new(),
        })))
    }

    /// C `getIntegerParam(scalerDone, ...)`.
    pub fn get(&self) -> i32 {
        self.0.lock().unwrap().value
    }

    /// C `setIntegerParam(scalerDone, value)`.
    pub fn set(&self, value: i32) {
        let mut st = self.0.lock().unwrap();
        if st.value != value {
            st.value = value;
            st.changed = true;
        }
    }

    /// C `callParamCallbacks` for this parameter.
    pub fn call_callbacks(&self) {
        let mut st = self.0.lock().unwrap();
        // One port reads one gate: the first user's database.
        if !st.changed || !st.users.first().is_some_and(|u| u.interrupt_accept()) {
            return;
        }
        st.changed = false;
        for user in &st.users {
            user.callback(st.value);
        }
    }

    /// C `registerInterruptUser`, plus the `callbackThread` flush when the
    /// user's database accepts interrupts.
    pub fn register(&self, user: DoneInterrupt) {
        self.0.lock().unwrap().users.push(user.clone());
        let param = self.clone();
        user.on_interrupt_accept(move || param.call_callbacks());
    }
}

/// Asyn-based device support for the scaler record — C `devScalerAsyn`.
///
/// C's dset is a set of synchronous entries the record calls from inside
/// `process()`; `init` binds the driver to the record as that dset
/// ([`ScalerRecord::bind_dset`]), so nothing is read before `process()` or
/// commanded after it. Completion arrives the other way: `init` registers the
/// record's [`DoneInterrupt`] with the driver, whose pulses process the record
/// regardless of `SCAN` (`deviceCallbackFunc`, `scalerRecord.c:191-200`).
pub struct ScalerAsynDeviceSupport {
    driver: Arc<Mutex<Box<dyn ScalerDriver>>>,
    accept: Arc<AcceptSlot>,
    pulse: mpsc::Sender<()>,
    pulses: Option<mpsc::Receiver<()>>,
}

impl ScalerAsynDeviceSupport {
    pub fn new(driver: Box<dyn ScalerDriver>) -> Self {
        Self::shared(Arc::new(Mutex::new(driver)))
    }

    /// Device support on a driver other records may share — one asyn port
    /// serving several scaler records.
    pub fn shared(driver: Arc<Mutex<Box<dyn ScalerDriver>>>) -> Self {
        let (pulse, pulses) = mpsc::channel(1);
        Self {
            driver,
            accept: Arc::default(),
            pulse,
            pulses: Some(pulses),
        }
    }

    pub fn driver(&self) -> Arc<Mutex<Box<dyn ScalerDriver>>> {
        Arc::clone(&self.driver)
    }
}

impl DeviceSupport for ScalerAsynDeviceSupport {
    fn dtyp(&self) -> &str {
        "Asyn Scaler"
    }

    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        let scaler = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<ScalerRecord>())
            .expect("ScalerAsynDeviceSupport requires a ScalerRecord");

        let mut driver = self.driver.lock().unwrap();
        // A custom `ScalerDriver` may report any channel count. Clamp to the
        // physical array bound so the record's fixed 64-element arrays are
        // never indexed out of range.
        scaler.nch = driver.num_channels().min(MAX_SCALER_CHANNELS) as i16;
        driver.register_done_interrupt(DoneInterrupt {
            done: scaler.done_latch(),
            pulse: self.pulse.clone(),
            accept: Arc::clone(&self.accept),
        });
        drop(driver);
        scaler.bind_dset(self.driver());
        Ok(DeviceInitOutcome::ok())
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn bind_interrupt_gate(&mut self, gate: &Arc<InterruptGate>) {
        self.accept.bind(gate);
    }

    fn io_intr_receiver(&mut self) -> Option<mpsc::Receiver<()>> {
        self.pulses.take()
    }

    /// `deviceCallbackFunc` processes the record whatever its `SCAN`.
    fn io_intr_scan_independent(&self) -> bool {
        true
    }
}
