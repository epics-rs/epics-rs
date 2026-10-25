//! Plugin runtime: control plane (PortActor) + data plane (processing thread).
//!
//! # Single-threaded data plane (intentional, G4)
//!
//! C++ `NDPluginDriver` runs `numThreads` worker threads sharing one input
//! queue (`createCallbackThreads`). The Rust port deliberately runs **exactly
//! one** per-plugin data thread driving a `tokio::select!` loop. This is an
//! intentional design choice: a single owner of the processing state removes
//! the C++ worker-pool races (shared `prevUniqueId_`, sort-buffer contention)
//! and keeps array ordering trivially correct. The `NUM_THREADS` / `MAX_THREADS`
//! PVs are therefore not backed by a real worker pool — instead `NumThreads`
//! is validated and clamped to `[1, MaxThreads]` on write and the clamped
//! value is written back, so the PV is honest about the accepted value rather
//! than silently inert.

// RTEMS-EXEC-MODEL-ALLOW(14): checked, not waived — all 14 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p ad-core-rs
// --all-features`, 578/578). ad-core-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself or, in `std_array_posts`, a current-thread runtime the test
// builds, neither of which the backend removes.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::{PortRuntimeHandle, create_port_runtime, port_runtime_unavailable};
use asyn_rs::trace::TraceMask;
use asyn_rs::user::AsynUser;
use epics_libcom_rs::runtime::task::{MandatoryThread, StackSizeClass, StackSpec, ThreadPriority};

use asyn_rs::port_handle::{CallbacksGate, PortHandle};

use crate::attributes::NDAttributeList;
use crate::driver::ndarray_driver::{
    ND_ARRAY_DRIVER_INTERFACES, ND_ARRAY_DRIVER_INTERRUPTS, PendingParam,
};
use crate::ndarray::NDArray;
use crate::ndarray_pool::{DriverPool, NDArrayPool, PublishedArray};
use crate::params::ndarray_driver::NDArrayDriverParams;
use asyn_rs::param::ParamValue;

use super::channel::{
    Admission, ArrayMessage, NDArrayOutput, NDArrayReceiver, NDArraySender, PublishOutcome,
    WeakNDArraySender, ndarray_channel,
};
use super::params::PluginBaseParams;
use super::wiring::WiringRegistry;

mod delivery;
use delivery::Delivery;

/// Message sent through the param channel from control plane to data plane.
///
/// The channel is FIFO, which is what gives [`PluginParamMsg::Barrier`] its
/// meaning: when the data thread acknowledges a barrier, every `Change`
/// enqueued before it has been fully applied (enable flips, wiring rewires,
/// processor param updates).
#[derive(Debug)]
enum PluginParamMsg {
    /// A param write to apply.
    Change(usize, i32, ParamChangeValue),
    /// Arrays a write handed to `endProcessCallbacks`, then the arrays it
    /// posted at an address (`doCallbacksGenericPointer`).
    Publish(Published, Vec<(usize, Arc<NDArray>)>),
    /// Sync barrier — acknowledged (best-effort send of `()`) at full
    /// quiescence: every `Change` enqueued before it has been applied (FIFO
    /// channel) AND the array queue has drained. The second condition exists
    /// because arrays travel a separate channel: without it, a param applied
    /// while an older array still waits in the queue would retroactively
    /// change how that array is processed.
    Barrier(std::sync::mpsc::SyncSender<()>),
    /// Acknowledged as soon as it is read, so every earlier `Change` has been
    /// applied but the array queue may still hold work.
    #[cfg(test)]
    Fence(std::sync::mpsc::SyncSender<()>),
}

/// Value sent through the param change channel from control plane to data plane.
#[derive(Debug, Clone)]
pub enum ParamChangeValue {
    Int32(i32),
    Float64(f64),
    Octet(String),
}

impl ParamChangeValue {
    pub fn as_i32(&self) -> i32 {
        match self {
            ParamChangeValue::Int32(v) => *v,
            ParamChangeValue::Float64(v) => *v as i32,
            ParamChangeValue::Octet(_) => 0,
        }
    }

    pub fn as_f64(&self) -> f64 {
        match self {
            ParamChangeValue::Int32(v) => *v as f64,
            ParamChangeValue::Float64(v) => *v,
            ParamChangeValue::Octet(_) => 0.0,
        }
    }

    pub fn as_string(&self) -> Option<&str> {
        match self {
            ParamChangeValue::Octet(s) => Some(s),
            _ => None,
        }
    }
}

/// A single parameter update produced by a plugin's process_array.
pub enum ParamUpdate {
    Int32 {
        reason: usize,
        addr: i32,
        value: i32,
    },
    Float64 {
        reason: usize,
        addr: i32,
        value: f64,
    },
    Octet {
        reason: usize,
        addr: i32,
        value: String,
    },
    Float64Array {
        reason: usize,
        addr: i32,
        value: Vec<f64>,
    },
    /// C `callParamCallbacks(addr)` where it stands.
    Callbacks { addr: i32 },
}

impl ParamUpdate {
    /// Create an Int32 update at addr 0.
    pub fn int32(reason: usize, value: i32) -> Self {
        Self::Int32 {
            reason,
            addr: 0,
            value,
        }
    }
    /// Create a Float64 update at addr 0.
    pub fn float64(reason: usize, value: f64) -> Self {
        Self::Float64 {
            reason,
            addr: 0,
            value,
        }
    }
    /// Create an Int32 update at a specific addr.
    pub fn int32_addr(reason: usize, addr: i32, value: i32) -> Self {
        Self::Int32 {
            reason,
            addr,
            value,
        }
    }
    /// Create a Float64 update at a specific addr.
    pub fn float64_addr(reason: usize, addr: i32, value: f64) -> Self {
        Self::Float64 {
            reason,
            addr,
            value,
        }
    }
    /// Create a Float64Array update at addr 0.
    pub fn float64_array(reason: usize, value: Vec<f64>) -> Self {
        Self::Float64Array {
            reason,
            addr: 0,
            value,
        }
    }
    /// Create a Float64Array update at a specific addr.
    pub fn float64_array_addr(reason: usize, addr: i32, value: Vec<f64>) -> Self {
        Self::Float64Array {
            reason,
            addr,
            value,
        }
    }
    /// Create an Octet (string) update at addr 0.
    pub fn octet(reason: usize, value: String) -> Self {
        Self::Octet {
            reason,
            addr: 0,
            value,
        }
    }
    /// Create an Octet (string) update at a specific addr.
    pub fn octet_addr(reason: usize, addr: i32, value: String) -> Self {
        Self::Octet {
            reason,
            addr,
            value,
        }
    }
    /// C `callParamCallbacks(addr)`.
    pub fn callbacks(addr: i32) -> Self {
        Self::Callbacks { addr }
    }

    /// The batch entry for this update: C's `set*Param`, or
    /// `doCallbacksFloat64Array` / `callParamCallbacks`, which post where they
    /// stand and store nothing (asynPortDriver.cpp:1829-1866).
    fn param_set(&self) -> asyn_rs::request::ParamSetValue {
        use asyn_rs::request::ParamSetValue;
        match self {
            Self::Int32 {
                reason,
                addr,
                value,
            } => ParamSetValue::new(*reason, *addr, ParamValue::Int32(*value)),
            Self::Float64 {
                reason,
                addr,
                value,
            } => ParamSetValue::new(*reason, *addr, ParamValue::Float64(*value)),
            Self::Octet {
                reason,
                addr,
                value,
            } => ParamSetValue::new(*reason, *addr, ParamValue::Octet(value.as_bytes().into())),
            Self::Float64Array {
                reason,
                addr,
                value,
            } => ParamSetValue::array(
                *reason,
                *addr,
                ParamValue::Float64Array(value.clone().into()),
            ),
            Self::Callbacks { addr } => ParamSetValue::callbacks(*addr),
        }
    }

    /// The `(reason, addr, value)` C stores; `None` for a post.
    pub(crate) fn stored(&self) -> Option<PendingParam> {
        match self.param_set() {
            asyn_rs::request::ParamSetValue::Value {
                reason,
                addr,
                value,
            } => Some((reason, addr, value)),
            _ => None,
        }
    }
}

/// The plugin's `NDAttributesFile` set, for a processor that calls C's
/// `getAttributes` itself (e.g. NDPluginTimeSeries.cpp:364).
#[derive(Clone)]
pub struct PluginAttributes(Arc<crate::driver::ndarray_driver::ArrayOwner>);

impl PluginAttributes {
    /// C `getAttributes(pList)`: merge the set, evaluated now, onto `list`.
    /// `pending` are the updates the processor will return for this frame,
    /// which C has already set when it makes the call.
    pub fn get_attributes(&self, list: &mut NDAttributeList, pending: &[ParamUpdate]) {
        if let Some(fresh) = self.evaluate(pending) {
            list.copy_from(&fresh);
        }
    }

    /// The set evaluated now, `None` when it is empty. Inside
    /// `process_array` the frame's `beginProcessCallbacks` sets count as
    /// pending ahead of `pending`.
    pub fn evaluate(&self, pending: &[ParamUpdate]) -> Option<NDAttributeList> {
        let mut sets = begin_now();
        sets.extend(pending.iter().filter_map(ParamUpdate::stored));
        self.0.evaluate_attributes_blocking(sets)
    }

    /// [`Self::evaluate`] with `begin`, the frame's `beginProcessCallbacks`
    /// sets, ahead of `pending`, from outside its `process_array`.
    fn evaluate_after(
        &self,
        begin: &[PendingParam],
        pending: &[ParamUpdate],
    ) -> Option<NDAttributeList> {
        let mut sets = begin.to_vec();
        sets.extend(pending.iter().filter_map(ParamUpdate::stored));
        self.0.evaluate_attributes_blocking(sets)
    }

    /// [`Self::evaluate`] from inside a write, on the port's actor, where
    /// `base` is the plugin's own.
    pub fn evaluate_on(&self, base: &PortDriverBase) -> Option<NDAttributeList> {
        let mut attributes = self.0.attributes().lock();
        (!attributes.list.is_empty()).then(|| attributes.update(base))
    }

    /// The store reads of [`Self::evaluate`], taken now, for a caller whose
    /// `getAttributes` comes after sets it cannot make here (a file plugin's
    /// `openFile`). `None` when the set is empty.
    fn view(&self) -> Option<AttributeView> {
        let begin = begin_now();
        let reads = self.0.param_reads_blocking(begin.clone())?;
        Some(AttributeView::new(Arc::clone(&self.0), reads, begin))
    }

    /// [`Self::view`] from inside a write, on the port's actor.
    fn view_on(&self, base: &PortDriverBase) -> Option<AttributeView> {
        let attributes = self.0.attributes().lock();
        (!attributes.list.is_empty()).then(|| {
            let reads = attributes.param_reads(base, &[]);
            let begin = begin_now();
            AttributeView::new(Arc::clone(&self.0), reads, begin)
        })
    }
}

/// What a plugin's `getAttributes` reads inside one frame callback or write:
/// the store read once, overlaid with every set made since, posted or not,
/// so it is evaluated on any thread without waiting on the port's actor.
/// Clones share the sets, so the poster, the writer and the hand-on read
/// one view.
#[derive(Clone)]
pub struct AttributeView {
    owner: Arc<crate::driver::ndarray_driver::ArrayOwner>,
    reads: Arc<crate::driver::ndarray_driver::ParamReads>,
    /// The frame's `beginProcessCallbacks` sets, then each set as made.
    made: Arc<parking_lot::Mutex<Vec<PendingParam>>>,
}

impl AttributeView {
    fn new(
        owner: Arc<crate::driver::ndarray_driver::ArrayOwner>,
        reads: crate::driver::ndarray_driver::ParamReads,
        begin: Vec<PendingParam>,
    ) -> Self {
        Self {
            owner,
            reads: Arc::new(reads),
            made: Arc::new(parking_lot::Mutex::new(begin)),
        }
    }

    /// C `setXxxParam`: `sets` are read from now on, whenever they post.
    pub fn record(&self, sets: impl IntoIterator<Item = PendingParam>) {
        self.made.lock().extend(sets);
    }

    /// The set as C `getAttributes` reads the parameter library now.
    pub fn evaluate(&self) -> NDAttributeList {
        let made = self.made.lock().clone();
        self.owner
            .attributes()
            .lock()
            .update_with(&self.reads, &made)
    }
}

/// Result of processing one array: output arrays + param updates to write back.
pub struct ProcessResult {
    pub output_arrays: Vec<Arc<NDArray>>,
    pub param_updates: Vec<ParamUpdate>,
    /// When `true`, the output arrays are *scattered* — delivered to a single
    /// downstream consumer in round-robin order rather than broadcast to all.
    /// The target consumer (and reroute-past-full / drop-on-last decisions) is
    /// owned by the runtime delivery path, which holds the persistent cursor
    /// (C++ `NDPluginScatter::nextClient_`); the processor only marks the frame
    /// as a scatter frame.
    pub scatter: bool,
    /// Arrays the plugin posts itself with C `doCallbacksGenericPointer(pArray,
    /// NDArrayData, addr)` (e.g. NDPluginStats.cpp:586 at address 1): each
    /// reaches only the subscribers of its address, and, being outside
    /// `endProcessCallbacks`, ignores NDArrayCallbacks, the throttle and the
    /// sort buffer. Published before `output_arrays`.
    pub addressed: Vec<(usize, Arc<NDArray>)>,
    /// Lines C's `processCallbacks` prints, in order.
    pub trace: Vec<(TraceMask, String)>,
    /// `false` where C's `processCallbacks` sets ArrayCounter back to the
    /// value it read before `beginProcessCallbacks` counted the frame, as
    /// NDPluginFile does for a frame it does not save (NDPluginFile.cpp:
    /// 727-731, :789).
    pub counted: bool,
}

impl ProcessResult {
    /// Convenience: sink plugin with only param updates, no output arrays.
    pub fn sink(param_updates: Vec<ParamUpdate>) -> Self {
        Self {
            output_arrays: vec![],
            param_updates,
            scatter: false,
            addressed: vec![],
            trace: vec![],
            counted: true,
        }
    }

    /// Convenience: transform plugin with output arrays but no param updates.
    pub fn arrays(output_arrays: Vec<Arc<NDArray>>) -> Self {
        Self {
            output_arrays,
            param_updates: vec![],
            scatter: false,
            addressed: vec![],
            trace: vec![],
            counted: true,
        }
    }

    /// Convenience: pass the input frame itself through, with `param_updates`
    /// — C's `doCallbacksGenericPointer(pArray, ...)` on the very array the
    /// plugin was handed. No copy: the output is the input `Arc`.
    pub fn forward(input: &Arc<NDArray>, param_updates: Vec<ParamUpdate>) -> Self {
        Self {
            output_arrays: vec![Arc::clone(input)],
            param_updates,
            scatter: false,
            addressed: vec![],
            trace: vec![],
            counted: true,
        }
    }

    /// Convenience: no outputs, no param updates.
    pub fn empty() -> Self {
        Self {
            output_arrays: vec![],
            param_updates: vec![],
            scatter: false,
            addressed: vec![],
            trace: vec![],
            counted: true,
        }
    }

    /// Convenience: scatter the input frame — deliver it to the next
    /// downstream consumer in round-robin order (the runtime owns the cursor
    /// and reroute logic).
    pub fn scatter(input: &Arc<NDArray>) -> Self {
        Self {
            output_arrays: vec![Arc::clone(input)],
            param_updates: vec![],
            scatter: true,
            addressed: vec![],
            trace: vec![],
            counted: true,
        }
    }
}

/// Run `processor` on one frame: the single writer of the processor's
/// served-array handle (what `readInt8Array` and friends hand out), which C's
/// `NDPluginStdArrays` keeps as its cached `pArrays[0]`.
fn run_processor<P: NDPluginProcess + ?Sized>(
    processor: &P,
    array: &Arc<NDArray>,
    pool: &NDArrayPool,
    begin: &[PendingParam],
    hand_on: Option<&HandOn>,
) -> ProcessResult {
    let result = with_frame(begin, hand_on, || processor.process_array(array, pool));
    if let Some(served) = processor.array_data_handle() {
        *served.lock() = result.output_arrays.first().cloned();
    }
    result
}

/// The `beginProcessCallbacks` sets of every frame whose `process_array` is
/// running on a thread, innermost last: a hand-on runs the next plugin's
/// inside the caller's.
struct BeginStack {
    sets: Vec<PendingParam>,
    /// Where the innermost frame's sets start.
    top: usize,
}

thread_local! {
    /// For the processor's own `getAttributes` calls, which come before the
    /// batch that stores the sets.
    static BEGIN: std::cell::RefCell<BeginStack> = const {
        std::cell::RefCell::new(BeginStack {
            sets: Vec::new(),
            top: 0,
        })
    };
    /// Where [`PortPost`] hands a frame on while `process_array` runs.
    static HAND_ON: std::cell::RefCell<Option<HandOn>> = const { std::cell::RefCell::new(None) };
}

/// The running frame's `beginProcessCallbacks` sets, none outside one.
fn begin_now() -> Vec<PendingParam> {
    BEGIN.with(|b| {
        let b = b.borrow();
        b.sets[b.top..].to_vec()
    })
}

fn with_frame<R>(begin: &[PendingParam], hand_on: Option<&HandOn>, f: impl FnOnce() -> R) -> R {
    /// The enclosing frame's `top`.
    struct Restore(usize, Option<HandOn>);
    impl Drop for Restore {
        fn drop(&mut self) {
            BEGIN.with(|b| {
                let b = &mut *b.borrow_mut();
                b.sets.truncate(b.top);
                b.top = self.0;
            });
            HAND_ON.with(|h| *h.borrow_mut() = self.1.take());
        }
    }
    let enclosing = BEGIN.with(|b| {
        let b = &mut *b.borrow_mut();
        let top = b.sets.len();
        b.sets.extend_from_slice(begin);
        std::mem::replace(&mut b.top, top)
    });
    let _restore = Restore(enclosing, HAND_ON.with(|h| h.replace(hand_on.cloned())));
    f()
}

/// Result of handling a control-plane param change.
pub struct ParamChangeResult {
    pub output_arrays: Vec<Arc<NDArray>>,
    pub param_updates: Vec<ParamUpdate>,
    /// As `ProcessResult::addressed`, posted from the write itself.
    pub addressed: Vec<(usize, Arc<NDArray>)>,
}

impl ParamChangeResult {
    pub fn updates(param_updates: Vec<ParamUpdate>) -> Self {
        Self {
            output_arrays: vec![],
            param_updates,
            addressed: vec![],
        }
    }

    pub fn arrays(output_arrays: Vec<Arc<NDArray>>) -> Self {
        Self {
            output_arrays,
            param_updates: vec![],
            addressed: vec![],
        }
    }

    pub fn combined(output_arrays: Vec<Arc<NDArray>>, param_updates: Vec<ParamUpdate>) -> Self {
        Self {
            output_arrays,
            param_updates,
            addressed: vec![],
        }
    }

    pub fn empty() -> Self {
        Self {
            output_arrays: vec![],
            param_updates: vec![],
            addressed: vec![],
        }
    }
}

/// Pure processing logic.
///
/// `process_array` and `on_param_change` take `&self` and the trait requires
/// `Sync` so that C's `NumThreads` worth of callback threads
/// (`NDPluginDriver.cpp:996-1001`) can run one processor concurrently. A
/// plugin that carries state owns the interior mutability for it — one lock
/// over the whole per-frame state, not one lock per field, so a frame's
/// reads and writes stay atomic against a concurrent frame.
/// What a write to a plugin port leaves in the parameter library.
#[derive(Debug, Clone, PartialEq)]
pub enum WriteVerdict<T> {
    /// Store this value (the written one, or the clamp C stores) and succeed.
    Accept(T),
    /// Store `keep` (C's clamp, or the old value C reverts to) and fail the
    /// write with `message`, as C's override returns `asynError`.
    Refuse { keep: T, message: String },
}

impl<T: Clone> WriteVerdict<T> {
    /// The value the param keeps.
    pub fn kept(&self) -> T {
        match self {
            Self::Accept(v) | Self::Refuse { keep: v, .. } => v.clone(),
        }
    }

    /// The override's verdict over the base class's `base`: a base refusal
    /// stands with its clamp, carrying the override's message if it refused
    /// too. C overrides that overwrite the base status (e.g.
    /// NDPluginStats.cpp:700) or store over its clamp (NDPluginCodec.cpp:1031)
    /// let a refused write succeed; not copied.
    fn under(self, base: Self) -> Self {
        match (base, self) {
            (Self::Refuse { keep, .. }, Self::Refuse { message, .. })
            | (Self::Refuse { keep, message }, Self::Accept(_)) => Self::Refuse { keep, message },
            (Self::Accept(_), own) => own,
        }
    }

    fn into_result(self) -> AsynResult<()> {
        match self {
            Self::Accept(_) => Ok(()),
            Self::Refuse { message, .. } => Err(write_refused(message)),
        }
    }
}

/// A param a C write override sets beside the one written, stored before
/// the same `callParamCallbacks`.
#[derive(Debug, Clone, PartialEq)]
pub enum SideParam {
    Int32 {
        reason: usize,
        addr: i32,
        value: i32,
    },
    Float64 {
        reason: usize,
        addr: i32,
        value: f64,
    },
    Octet {
        reason: usize,
        addr: i32,
        value: String,
    },
    /// C `doCallbacksFloat64Array`: posted at once, stored nowhere.
    Float64Array {
        reason: usize,
        addr: i32,
        value: Vec<f64>,
    },
    /// C `callParamCallbacks(addr)`, at once.
    Callbacks { addr: i32 },
}

impl From<ParamUpdate> for SideParam {
    fn from(update: ParamUpdate) -> Self {
        match update {
            ParamUpdate::Int32 {
                reason,
                addr,
                value,
            } => Self::Int32 {
                reason,
                addr,
                value,
            },
            ParamUpdate::Float64 {
                reason,
                addr,
                value,
            } => Self::Float64 {
                reason,
                addr,
                value,
            },
            ParamUpdate::Octet {
                reason,
                addr,
                value,
            } => Self::Octet {
                reason,
                addr,
                value,
            },
            ParamUpdate::Float64Array {
                reason,
                addr,
                value,
            } => Self::Float64Array {
                reason,
                addr,
                value,
            },
            ParamUpdate::Callbacks { addr } => Self::Callbacks { addr },
        }
    }
}

/// What a C write override leaves behind: the written param's verdict, the
/// params it sets beside it, and the `asynPrint` lines it prints. The port
/// applies all three; a `validate_*` hook never writes the port itself.
/// `settled` means the override did everything C does for this write here,
/// so `on_param_change` never sees it (a revert, or a start that failed).
#[derive(Debug, Clone, PartialEq)]
pub struct WriteOutcome<T> {
    pub verdict: WriteVerdict<T>,
    pub side: Vec<SideParam>,
    pub trace: Vec<(TraceMask, String)>,
    pub settled: bool,
    pub publish: Published,
    pub addressed: Addressed,
}

/// C `endProcessCallbacks`' `copyArray`.
#[derive(Clone, Copy)]
enum CopyArray<'a> {
    /// Copy an output that is this input, as every C plugin handing on its
    /// input asks (e.g. NDPluginStats.cpp:651, NDPluginCodec.cpp:987), and
    /// not one of the plugin's own (e.g. NDPluginROI.cpp:226).
    IfInput(&'a NDArray),
    Always,
    Never,
}

/// Arrays a write hands to `endProcessCallbacks`, compared by identity.
#[derive(Debug, Clone, Default)]
pub enum Published {
    #[default]
    Nothing,
    /// ReadFile's array: `beginProcessCallbacks` counts it, and it goes out
    /// uncopied (NDPluginFile.cpp:190-193).
    Read(Arc<NDArray>),
}

impl PartialEq for Published {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Nothing, Self::Nothing) => true,
            (Self::Read(a), Self::Read(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// Arrays a write posts at an address (C `doCallbacksGenericPointer(array,
/// NDArrayData, addr)`), compared by identity.
#[derive(Debug, Clone, Default)]
pub struct Addressed(pub Vec<(usize, Arc<NDArray>)>);

impl PartialEq for Addressed {
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len()
            && self
                .0
                .iter()
                .zip(&other.0)
                .all(|((a, x), (b, y))| a == b && Arc::ptr_eq(x, y))
    }
}

impl<T> From<WriteVerdict<T>> for WriteOutcome<T> {
    fn from(verdict: WriteVerdict<T>) -> Self {
        Self {
            verdict,
            side: Vec::new(),
            trace: Vec::new(),
            settled: false,
            publish: Published::default(),
            addressed: Addressed::default(),
        }
    }
}

impl<T> WriteOutcome<T> {
    pub fn set_int32(mut self, reason: usize, addr: i32, value: i32) -> Self {
        self.side.push(SideParam::Int32 {
            reason,
            addr,
            value,
        });
        self
    }

    pub fn set_float64(mut self, reason: usize, addr: i32, value: f64) -> Self {
        self.side.push(SideParam::Float64 {
            reason,
            addr,
            value,
        });
        self
    }

    pub fn set_octet(mut self, reason: usize, addr: i32, value: impl Into<String>) -> Self {
        self.side.push(SideParam::Octet {
            reason,
            addr,
            value: value.into(),
        });
        self
    }

    pub fn print(mut self, mask: TraceMask, line: impl Into<String>) -> Self {
        self.trace.push((mask, line.into()));
        self
    }

    pub fn settled(mut self) -> Self {
        self.settled = true;
        self
    }

    /// Store the updates a shared helper built, as side params.
    pub fn set_updates(mut self, updates: Vec<ParamUpdate>) -> Self {
        self.side.extend(updates.into_iter().map(SideParam::from));
        self
    }

    pub fn publish_read(mut self, array: Arc<NDArray>) -> Self {
        self.publish = Published::Read(array);
        self
    }

    pub fn post_addressed(mut self, posts: Vec<(usize, Arc<NDArray>)>) -> Self {
        self.addressed.0.extend(posts);
        self
    }
}

/// The callbacks a C write override ends in. NDPluginDriver's own write
/// ends in `callParamCallbacks(addr)` (NDPluginDriver.cpp:767, :809, :845),
/// and an override hands it the base class's reasons first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteEpilogue {
    /// No override, or one ending as NDPluginDriver does.
    Addr,
    /// `callParamCallbacks(addr)` then `callParamCallbacks()`.
    AddrAndZero,
    /// `setIntegerParam(function, value)` and `callParamCallbacks()`: list 0
    /// alone, whatever address was written.
    Zero,
}

impl WriteEpilogue {
    /// The address posted first, if any, and whether address 0 follows, for
    /// a write at `addr`; `own` is a reason the override keeps rather than
    /// handing to NDPluginDriver.
    fn posts(self, addr: i32, own: bool) -> (Option<i32>, bool) {
        match self {
            Self::Addr => (Some(addr), false),
            Self::AddrAndZero => (Some(addr), true),
            Self::Zero => ((!own).then_some(addr), true),
        }
    }

    /// Where the written value is stored, in the same shape: a `Zero`
    /// override stores at list 0 (NDPluginAttribute.cpp:121,
    /// NDPosPlugin.cpp:228) and NDPluginDriver stores a reason handed to it
    /// at `addr`.
    fn stores(self, addr: i32, own: bool) -> (Option<i32>, bool) {
        match self {
            Self::Zero => ((!own).then_some(addr), true),
            Self::Addr | Self::AddrAndZero => (Some(addr), false),
        }
    }
}

/// Store one param a write override set; an array posts at once and is
/// not stored, as `doCallbacksFloat64Array` does.
fn set_side(base: &mut PortDriverBase, p: SideParam) -> AsynResult<()> {
    match p {
        SideParam::Int32 {
            reason,
            addr,
            value,
        } => base.set_int32_param(reason, addr, value),
        SideParam::Float64 {
            reason,
            addr,
            value,
        } => base.set_float64_param(reason, addr, value),
        SideParam::Octet {
            reason,
            addr,
            value,
        } => base.set_string_param(reason, addr, &value),
        SideParam::Float64Array {
            reason,
            addr,
            value,
        } => base.do_callbacks_array(reason, addr, ParamValue::Float64Array(value.into())),
        SideParam::Callbacks { addr } => base.call_param_callbacks(addr),
    }
}

/// C `set*Param` + `callParamCallbacks()` in the middle of a plugin's work,
/// posted at once rather than with the rest of the frame callback or write:
/// NDPluginFile's capture flush posts NumCaptured per frame
/// (NDPluginFile.cpp:278-279, :315-321).
pub trait ParamPost {
    fn post(&mut self, updates: Vec<ParamUpdate>);

    /// C `endProcessCallbacks(array, true, true)` from inside the work
    /// (NDPluginFile.cpp:252, :301, :349): `array` is delivered downstream
    /// before this returns, and the counters C sets there go out with the
    /// next post.
    fn hand_on(&mut self, array: &Arc<NDArray>);

    /// The view the hand-on's `getAttributes` reads, which records each
    /// post; a caller records its own sets into it as C makes them.
    fn attribute_view(&mut self) -> Option<AttributeView> {
        None
    }

    /// C `getIntegerParam(reason)` at address 0 now, where the poster can
    /// read it.
    fn int32(&self, _reason: usize) -> Option<i32> {
        None
    }
}

/// C `endProcessCallbacks(pArray, true, true)` called from inside the
/// work, as NDPluginFile's `writeFileBase` calls it for each frame it wrote
/// (NDPluginFile.cpp:252, :301, :349): no `beginProcessCallbacks` counts the
/// frame, and it is delivered before the next `callParamCallbacks`.
#[derive(Clone)]
pub struct HandOn {
    delivery: Arc<Delivery>,
    attributes: PluginAttributes,
    plugin_params: PluginBaseParams,
}

impl HandOn {
    fn throttled(&self, array: &NDArray) -> bool {
        self.delivery.throttled(array)
    }

    /// Deliver `array` with what `attributes` reads when NDArrayCallbacks
    /// is on (NDPluginDriver.cpp:289); returns the counters C sets there.
    fn frame(
        &self,
        array: &Arc<NDArray>,
        attributes: impl FnOnce(&PluginAttributes) -> Option<NDAttributeList>,
    ) -> Vec<ParamUpdate> {
        let attributes = self
            .delivery
            .delivers()
            .then(|| attributes(&self.attributes))
            .flatten();
        self.delivery.hand_on(array, attributes.as_ref());
        let counters = self.delivery.sort_counters();
        let p = &self.plugin_params;
        let mut sets = vec![
            ParamUpdate::int32(p.disordered_arrays, counters.disordered_arrays),
            ParamUpdate::int32(p.dropped_output_arrays, counters.dropped_output_arrays),
        ];
        sets.extend(
            counters
                .sort_free
                .map(|free| ParamUpdate::int32(p.sort_free, free)),
        );
        sets
    }
}

/// [`ParamPost`] from a frame callback, through the port's queue, ahead of
/// the callback's own updates, which the same queue carries after it.
///
/// The first post carries the frame's `beginProcessCallbacks` sets, which C
/// has made by then and its `callParamCallbacks` posts with it.
///
/// A hand-on's `getAttributes` is evaluated on the calling thread, as C's
/// is, from its [`AttributeView`], so a caller may hand on while holding a
/// lock the actor's writes wait for.
pub struct PortPost {
    port: Option<PortHandle>,
    begin: Vec<PendingParam>,
    /// The begin sets, kept to read after they are posted.
    read: Vec<PendingParam>,
    hand_on: Option<HandOn>,
    /// What a hand-on set, for the next post.
    set: Vec<ParamUpdate>,
    view: Option<AttributeView>,
}

impl PortPost {
    /// The port of `attributes`' driver; with none, posts go nowhere.
    ///
    /// Reads the store on the port's actor, so it must be made before
    /// taking a lock the actor's writes take.
    pub fn of(attributes: Option<&PluginAttributes>) -> Self {
        let begin = begin_now();
        Self {
            port: attributes.and_then(|a| a.0.port()),
            read: begin.clone(),
            begin,
            hand_on: HAND_ON.with(|h| h.borrow().clone()),
            set: Vec::new(),
            view: attributes.and_then(PluginAttributes::view),
        }
    }

    /// C `NDPluginDriver::throttled`, for a plugin whose output is not an
    /// NDArray and so is not throttled by `endProcessCallbacks`
    /// (NDPluginPva.cpp:97-99). A refusal is counted in
    /// DroppedOutputArrays.
    pub fn throttled(&self, array: &NDArray) -> bool {
        self.hand_on.as_ref().is_some_and(|h| h.throttled(array))
    }
}

impl ParamPost for PortPost {
    fn post(&mut self, updates: Vec<ParamUpdate>) {
        let Some(port) = &self.port else { return };
        let updates: Vec<ParamUpdate> = std::mem::take(&mut self.set)
            .into_iter()
            .chain(updates)
            .collect();
        let begin = std::mem::take(&mut self.begin);
        if let Some(view) = &self.view {
            view.record(
                begin
                    .iter()
                    .cloned()
                    .chain(updates.iter().filter_map(ParamUpdate::stored)),
            );
        }
        let sets = begin
            .into_iter()
            .map(|(reason, addr, value)| asyn_rs::request::ParamSetValue::new(reason, addr, value))
            .chain(updates.iter().map(ParamUpdate::param_set))
            .collect();
        if let Err(e) = port.set_params_and_notify_blocking(0, sets) {
            epics_libcom_rs::stderr_println!("plugin param post error: {e}");
        }
    }

    fn hand_on(&mut self, array: &Arc<NDArray>) {
        let Some(hand_on) = &self.hand_on else { return };
        let view = &self.view;
        let sets = hand_on.frame(array, |_| view.as_ref().map(AttributeView::evaluate));
        if let Some(view) = view {
            view.record(sets.iter().filter_map(ParamUpdate::stored));
        }
        self.set.extend(sets);
    }

    fn attribute_view(&mut self) -> Option<AttributeView> {
        self.view.clone()
    }

    /// Only the frame's `beginProcessCallbacks` sets, ArrayCounter among
    /// them, are known here.
    fn int32(&self, reason: usize) -> Option<i32> {
        self.read.iter().find_map(|(r, addr, value)| match value {
            ParamValue::Int32(v) if *r == reason && *addr == 0 => Some(*v),
            _ => None,
        })
    }
}

/// The port an int32 write override runs against, on its actor: the
/// parameter library before the store, and [`ParamPost`] into it.
pub struct WritePort<'a> {
    base: &'a mut PortDriverBase,
    hand_on: Option<&'a HandOn>,
    /// Read from the store when first asked for, then given each post.
    view: Option<AttributeView>,
}

impl<'a> WritePort<'a> {
    /// A port with nothing downstream.
    pub fn new(base: &'a mut PortDriverBase) -> Self {
        Self {
            base,
            hand_on: None,
            view: None,
        }
    }
}

impl std::ops::Deref for WritePort<'_> {
    type Target = PortDriverBase;

    fn deref(&self) -> &PortDriverBase {
        self.base
    }
}

impl ParamPost for WritePort<'_> {
    fn post(&mut self, updates: Vec<ParamUpdate>) {
        if let Some(view) = &self.view {
            view.record(updates.iter().filter_map(ParamUpdate::stored));
        }
        for update in updates {
            let _ = set_side(self.base, update.into());
        }
        let _ = self.base.call_param_callbacks(0);
    }

    /// The actor waits for a blocking downstream, as C's port thread runs
    /// it inline under the port lock.
    fn hand_on(&mut self, array: &Arc<NDArray>) {
        let Some(hand_on) = self.hand_on else { return };
        let view = self.attribute_view();
        let sets = hand_on.frame(array, |_| view.as_ref().map(AttributeView::evaluate));
        if let Some(view) = &view {
            view.record(sets.iter().filter_map(ParamUpdate::stored));
        }
        for set in sets {
            let _ = set_side(self.base, set.into());
        }
    }

    fn attribute_view(&mut self) -> Option<AttributeView> {
        if self.view.is_none() {
            self.view = self.hand_on.and_then(|h| h.attributes.view_on(self.base));
        }
        self.view.clone()
    }

    fn int32(&self, reason: usize) -> Option<i32> {
        self.base.get_int32_param(reason, 0).ok()
    }
}

/// Posts nowhere and has nothing downstream, for a caller with no port.
impl ParamPost for () {
    fn post(&mut self, _updates: Vec<ParamUpdate>) {}
    fn hand_on(&mut self, _array: &Arc<NDArray>) {}
}

/// Keeps each post, for a caller with no port and nothing downstream.
impl ParamPost for Vec<Vec<ParamUpdate>> {
    fn post(&mut self, updates: Vec<ParamUpdate>) {
        self.push(updates);
    }
    fn hand_on(&mut self, _array: &Arc<NDArray>) {}
}

/// The `validate_*` half of an `NDPluginProcess` and its edge posts,
/// callable through the port driver, which does not know the plugin's type.
trait ValidateWrite: Send + Sync {
    fn int32(
        &self,
        reason: usize,
        addr: i32,
        value: i32,
        verdict: WriteVerdict<i32>,
        base: &mut PortDriverBase,
        hand_on: Option<&HandOn>,
    ) -> WriteOutcome<i32>;
    fn float64(
        &self,
        reason: usize,
        addr: i32,
        value: f64,
        verdict: WriteVerdict<f64>,
        base: &PortDriverBase,
    ) -> WriteOutcome<f64>;
    fn octet(
        &self,
        reason: usize,
        addr: i32,
        value: &str,
        verdict: WriteVerdict<String>,
        base: &PortDriverBase,
    ) -> WriteOutcome<String>;
    fn posts_before_write(&self, reason: usize, value: i32) -> bool;
    fn edge_posts(&self) -> Vec<ParamUpdate>;
}

impl<P: NDPluginProcess> ValidateWrite for P {
    fn int32(
        &self,
        reason: usize,
        addr: i32,
        value: i32,
        verdict: WriteVerdict<i32>,
        base: &mut PortDriverBase,
        hand_on: Option<&HandOn>,
    ) -> WriteOutcome<i32> {
        self.validate_int32(
            reason,
            addr,
            value,
            verdict,
            &mut WritePort {
                base,
                hand_on,
                view: None,
            },
        )
    }
    fn float64(
        &self,
        reason: usize,
        addr: i32,
        value: f64,
        verdict: WriteVerdict<f64>,
        base: &PortDriverBase,
    ) -> WriteOutcome<f64> {
        self.validate_float64(reason, addr, value, verdict, base)
    }
    fn octet(
        &self,
        reason: usize,
        addr: i32,
        value: &str,
        verdict: WriteVerdict<String>,
        base: &PortDriverBase,
    ) -> WriteOutcome<String> {
        self.validate_octet(reason, addr, value, verdict, base)
    }
    fn posts_before_write(&self, reason: usize, value: i32) -> bool {
        NDPluginProcess::posts_before_write(self, reason, value)
    }
    fn edge_posts(&self) -> Vec<ParamUpdate> {
        #[allow(deprecated)]
        NDPluginProcess::edge_posts(self)
    }
}

/// The interfaces `NDPluginDriver` ORs into its plugin's before
/// `asynNDArrayDriver` adds its own (NDPluginDriver.cpp:91).
pub const PLUGIN_DRIVER_INTERFACES: InterfaceMask = InterfaceMask::INT32
    .or(InterfaceMask::FLOAT64)
    .or(InterfaceMask::OCTET)
    .or(InterfaceMask::INT32_ARRAY)
    .or(InterfaceMask::DRV_USER);

/// The interrupts `NDPluginDriver` ORs into its plugin's (NDPluginDriver.cpp:92).
pub const PLUGIN_DRIVER_INTERRUPTS: InterfaceMask = InterfaceMask::INT32
    .or(InterfaceMask::FLOAT64)
    .or(InterfaceMask::OCTET)
    .or(InterfaceMask::INT32_ARRAY);

pub trait NDPluginProcess: Send + Sync + 'static {
    /// The `asynFlags` C's plugin hands its `NDPluginDriver` constructor:
    /// only a can-block port gets a port thread, at the `*Configure` line's
    /// priority and stackSize (asynManager.c:2074). No default, as C's
    /// constructor argument has none.
    const ASYN_FLAGS: PortFlags;

    /// The `interfaceMask` / `interruptMask` C's plugin hands its
    /// `NDPluginDriver` constructor, which ORs in its own (NDPluginDriver.cpp:91).
    const INTERFACE_MASK: InterfaceMask;
    const INTERRUPT_MASK: InterfaceMask;

    /// The callbacks C's writeInt32 override ends in; a plugin without one
    /// ends as NDPluginDriver does.
    const WRITE_INT32_EPILOGUE: WriteEpilogue = WriteEpilogue::Addr;
    /// As `WRITE_INT32_EPILOGUE`, for writeFloat64.
    const WRITE_FLOAT64_EPILOGUE: WriteEpilogue = WriteEpilogue::Addr;

    /// Process one array. Return output arrays and param updates.
    ///
    /// May run concurrently with itself on `NumThreads` worker threads.
    fn process_array(&self, array: &Arc<NDArray>, pool: &NDArrayPool) -> ProcessResult;

    /// Plugin type name for PLUGIN_TYPE param.
    fn plugin_type(&self) -> &str;

    /// Whether this plugin can process compressed (`codec != None`) arrays
    /// (C++ `compressionAware_`, G3). Defaults to `false`: a plugin that
    /// operates on raw pixels must not be handed compressed bytes — the
    /// runtime drops compressed input and counts it into DroppedArrays.
    /// A codec/file plugin that understands compressed data overrides this.
    fn compression_aware(&self) -> bool {
        false
    }

    /// Whether the frame is for this plugin at all. C `NDPluginFile`'s
    /// `attrIsProcessingRequired` returns before `beginProcessCallbacks`
    /// (NDPluginFile.cpp:724-725), so a frame refused here counts nothing,
    /// sets nothing but ExecutionTime, and is never cached or handed on.
    fn processing_required(&self, _array: &NDArray) -> bool {
        true
    }

    /// The `NDArrayCallbacks` value this plugin's C constructor sets: 0 in
    /// NDPluginFile/StdArrays/Attribute (e.g. NDPluginFile.cpp:948), 1 in
    /// Codec/CircularBuff/ColorConvert/Process/Transform/BadPixel/Overlay.
    /// `None`, the default, is every other plugin, which leaves it to the
    /// template.
    fn array_callbacks_seed(&self) -> Option<bool> {
        None
    }

    /// Hands the processor a read view of the runtime's NDArrayCallbacks, for
    /// work C does only inside its own `if (arrayCallbacks)` (e.g.
    /// NDPluginTimeSeries.cpp:341-385). Called once, before the port exists.
    fn bind_array_callbacks(&mut self, _array_callbacks: ArrayCallbacks) {}

    /// C `endProcessCallbacks`' `readAttributes` (NDPluginDriver.cpp:289):
    /// whether the runtime merges the plugin's attribute set onto each
    /// output. A plugin that passes `false` calls `getAttributes` itself,
    /// through [`Self::bind_attributes`].
    const READ_ATTRIBUTES: bool = true;

    /// Hands the processor its plugin's attribute set, for the
    /// `getAttributes` calls C makes inside `processCallbacks`. Called once,
    /// before the port exists.
    fn bind_attributes(&mut self, _attributes: PluginAttributes) {}

    /// Hands the processor C's `pNDArrayPool`, for arrays it keeps across
    /// frames (e.g. NDPluginTimeSeries.cpp:132). Called once, after
    /// `register_params`, with the plugin's own pool in it.
    fn bind_pool(&mut self, _pool: PluginPool) {}

    /// How many `(NDArrayPort, NDArrayAddr)` pairs this plugin reads as its
    /// array sources: the pair at address `a` for every `a` below this.
    /// `NDPluginDriver::connectToArrayPort` reads the pair at address 0
    /// (NDPluginDriver.cpp:590-591); `NDPluginGather` overrides it to read
    /// one pair per address in `0..maxPorts_` and connects to every one
    /// (NDPluginGather.cpp:146-148).
    fn num_array_sources(&self) -> usize {
        1
    }

    /// The `maxAddr` C's plugin hands its `NDPluginDriver` constructor
    /// (NDPluginDriver.cpp:85,90): the port's addresses, each with its own
    /// parameter list and array output. 1 unless the plugin sizes per-address
    /// state by a `*Configure` count.
    fn max_addr(&self) -> usize {
        1
    }

    /// Register plugin-specific params on the base. Called once during construction.
    fn register_params(
        &mut self,
        _base: &mut PortDriverBase,
    ) -> Result<(), asyn_rs::error::AsynError> {
        Ok(())
    }

    /// The synchronous half of a C plugin's `writeInt32` override: run on the
    /// port before the store, it decides what the param keeps, whether the
    /// write fails, and what else C sets and prints on the way. `verdict` is
    /// what C `NDPluginDriver::writeInt32` leaves for the base class params
    /// (`Accept(value)` for every other param), as the override that calls
    /// it sees the status; a base refusal stands, with its clamp, whatever
    /// the override returns. `params` is the parameter library before the
    /// store, which the override posts into only where C calls back in the
    /// middle of the write. `on_param_change` then sees the kept value. The default keeps
    /// the base verdict, as a plugin with no override does.
    fn validate_int32(
        &self,
        _reason: usize,
        _addr: i32,
        _value: i32,
        verdict: WriteVerdict<i32>,
        _params: &mut WritePort<'_>,
    ) -> WriteOutcome<i32> {
        verdict.into()
    }

    /// `validate_int32` for a C `writeFloat64` override.
    fn validate_float64(
        &self,
        _reason: usize,
        _addr: i32,
        _value: f64,
        verdict: WriteVerdict<f64>,
        _params: &PortDriverBase,
    ) -> WriteOutcome<f64> {
        verdict.into()
    }

    /// `validate_int32` for a C `writeOctet` override.
    fn validate_octet(
        &self,
        _reason: usize,
        _addr: i32,
        _value: &str,
        verdict: WriteVerdict<String>,
        _params: &PortDriverBase,
    ) -> WriteOutcome<String> {
        verdict.into()
    }

    /// Whether C's `writeInt32` stores and posts the written value before
    /// the work `validate_int32` does, as `NDPluginFile` does for a busy
    /// `WriteFile`/`ReadFile` so its 1 -> 0 reaches the record.
    fn posts_before_write(&self, _reason: usize, _value: i32) -> bool {
        false
    }

    /// Called when a param changes. Reason is the param index.
    /// Return param updates to be written back to the port driver.
    fn on_param_change(&self, _reason: usize, _params: &PluginParamSnapshot) -> ParamChangeResult {
        ParamChangeResult::empty()
    }

    /// The arrays posted again when the port's interrupts open
    /// ([`PortDriver::interrupts_opened`]).
    #[deprecated(
        note = "mark the reason with `PortDriverBase::set_state_array_rule`; removed in 0.33"
    )]
    fn edge_posts(&self) -> Vec<ParamUpdate> {
        Vec::new()
    }

    /// Return a handle to the latest NDArray data for array reads.
    /// Override this in plugins like NDPluginStdArrays that serve pixel data
    /// via readInt8Array/readInt16Array/etc.
    fn array_data_handle(&self) -> Option<Arc<parking_lot::Mutex<Option<Arc<NDArray>>>>> {
        None
    }
}

/// NDArrayCallbacks as the runtime holds it. The runtime's ARRAY_CALLBACKS
/// write is its only writer; a processor gets clones that can only read.
#[derive(Clone, Debug)]
pub struct ArrayCallbacks(Arc<AtomicBool>);

impl ArrayCallbacks {
    /// A view that holds `on` until a runtime binds its own in its place.
    pub fn new(on: bool) -> Self {
        Self(Arc::new(AtomicBool::new(on)))
    }

    fn set(&self, on: bool) {
        self.0.store(on, Ordering::Release);
    }

    pub fn get(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// C `pNDArrayPool`: the plugin's own pool until a frame brings another
/// (NDPluginDriver.cpp:419). `Delivery::adopt_pool` is its only writer;
/// a processor gets clones that can only read.
#[derive(Clone)]
pub struct PluginPool(Arc<parking_lot::Mutex<DriverPool>>);

impl PluginPool {
    /// A view that holds `pool` until a runtime binds its own in its place.
    pub fn new(pool: DriverPool) -> Self {
        Self(Arc::new(parking_lot::Mutex::new(pool)))
    }

    fn adopt(&self, pool: &DriverPool) {
        *self.0.lock() = pool.clone();
    }

    pub fn current(&self) -> DriverPool {
        self.0.lock().clone()
    }
}

/// Read-only snapshot of param values available to the processing thread.
pub struct PluginParamSnapshot {
    pub enable_callbacks: bool,
    /// The param reason that changed.
    pub reason: usize,
    /// The address (sub-device) that changed.
    pub addr: i32,
    /// The new value.
    pub value: ParamChangeValue,
}

/// One buffered entry in the sort buffer: the output arrays for a uniqueId
/// plus the instant they were inserted (for the per-element staleness
/// deadline — C++ `sortedListElement::insertionTime_`).
struct SortEntry {
    arrays: Vec<PublishedArray>,
    inserted: std::time::Instant,
}

/// Sort buffer for reordering out-of-order output arrays by uniqueId.
///
/// Port of C++ `sortedNDArrayList_` semantics (NDPluginDriver.cpp).
/// Only arrays that arrive *out of order* are buffered here — in-order
/// arrays are emitted immediately by the caller (B2). The drain logic
/// (`drain_ready`) releases the head while the next-expected uniqueId is
/// contiguous OR the head has been buffered longer than `sort_time` (B3).
struct SortBuffer {
    /// Buffered out-of-order arrays keyed by uniqueId.
    entries: BTreeMap<i32, SortEntry>,
    /// uniqueId of the last array emitted downstream (C++ `prevUniqueId_`).
    ///
    /// Starts at C's sentinel so that "nothing emitted yet" is expressed here
    /// and nowhere else: no real uniqueId is adjacent to it, so the first
    /// output array is out of order by the same rule every later one is
    /// judged by, and no caller needs a separate "is this the first?" branch.
    prev_unique_id: i32,
    /// Whether any array has been emitted yet (C++ `firstOutputArray_`).
    first_output: bool,
    /// Cumulative count of arrays emitted out of order (C++ DisorderedArrays).
    disordered_arrays: i32,
    /// Cumulative count of arrays dropped because the buffer was full
    /// (C++ DroppedOutputArrays — sort-buffer-overflow portion).
    dropped_output_arrays: i32,
}

impl SortBuffer {
    fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            prev_unique_id: -1000,
            first_output: true,
            disordered_arrays: 0,
            dropped_output_arrays: 0,
        }
    }

    /// True if `unique_id` follows `prev_unique_id` in order (C++ `orderOK`).
    fn order_ok(&self, unique_id: i32) -> bool {
        unique_id == self.prev_unique_id || unique_id == self.prev_unique_id + 1
    }

    /// Record that an array with `unique_id` was emitted downstream.
    /// Updates `prev_unique_id` and counts a disorder if it was out of order.
    fn note_emitted(&mut self, unique_id: i32) {
        if !self.first_output && !self.order_ok(unique_id) {
            self.disordered_arrays += 1;
        }
        self.first_output = false;
        self.prev_unique_id = unique_id;
    }

    /// Insert an out-of-order array into the sort buffer.
    ///
    /// Returns `false` if the buffer was full and the array was dropped
    /// (C++ NDPluginDriver.cpp:307-316), `true` if buffered.
    fn insert(&mut self, unique_id: i32, arrays: Vec<PublishedArray>, sort_size: i32) -> bool {
        if sort_size > 0 && self.entries.len() as i32 >= sort_size {
            self.dropped_output_arrays += 1;
            return false;
        }
        self.entries
            .entry(unique_id)
            .or_insert_with(|| SortEntry {
                arrays: Vec::new(),
                inserted: std::time::Instant::now(),
            })
            .arrays
            .extend(arrays);
        true
    }

    /// Drain the buffer head-first while either the next expected uniqueId is
    /// contiguous OR the head element has aged past `sort_time` seconds at
    /// `now`. Port of C++ `sortingTask` loop (NDPluginDriver.cpp:619-670).
    fn drain_ready(
        &mut self,
        sort_time: f64,
        now: std::time::Instant,
    ) -> Vec<(i32, Vec<PublishedArray>)> {
        let mut out = Vec::new();
        while let Some((&head_id, entry)) = self.entries.iter().next() {
            let delta = now.duration_since(entry.inserted).as_secs_f64();
            let order_ok = self.order_ok(head_id);
            if (!self.first_output && order_ok) || delta > sort_time {
                let entry = self.entries.remove(&head_id).unwrap();
                self.note_emitted(head_id);
                out.push((head_id, entry.arrays));
            } else {
                break;
            }
        }
        out
    }

    /// Drain every buffered array in uniqueId order, regardless of contiguity
    /// or age. Used when sort mode is turned off.
    fn drain_all(&mut self) -> Vec<(i32, Vec<PublishedArray>)> {
        let entries = std::mem::take(&mut self.entries);
        let mut out = Vec::with_capacity(entries.len());
        for (id, entry) in entries {
            self.note_emitted(id);
            out.push((id, entry.arrays));
        }
        out
    }

    /// Number of uniqueId entries currently buffered.
    fn len(&self) -> i32 {
        self.entries.len() as i32
    }
}

/// C `beginProcessCallbacks`' sets (NDPluginDriver.cpp:216-235).
type BeginSets = [PendingParam; 11];

/// Shared processor state protected by a mutex, accessible from both
/// the data thread (non-blocking mode) and the caller thread (blocking mode).
struct SharedProcessorInner {
    /// The outputs, pool, throttle and sort buffer `endProcessCallbacks`
    /// works on, behind their own leaf lock.
    delivery: Arc<Delivery>,
    /// The plugin's own attribute set, and whether `endProcessCallbacks`
    /// reads it ([`NDPluginProcess::READ_ATTRIBUTES`]).
    attributes: PluginAttributes,
    read_attributes: bool,
    ndarray_params: NDArrayDriverParams,
    plugin_params: PluginBaseParams,
    port_handle: PortHandle,
    /// ArrayCounter — owned in the param library (C++ `NDArrayCounter`), held
    /// here only as a working copy that is kept in sync with the param so a
    /// control-plane write of `ARRAY_COUNTER` resets it (B12).
    array_counter: i32,
    /// Param index for STD_ARRAY_DATA (if this is a StdArrays plugin).
    std_array_data_param: Option<usize>,
    /// NDArrayCallbacks (C++ `NDArrayCallbacks`): when `false`, the plugin
    /// still processes and updates its metadata params but does NOT deliver the
    /// output array downstream — `endProcessCallbacks` (NDPluginDriver.cpp:
    /// 257-265) returns before the sort/throttle/`doCallbacksGenericPointer`
    /// path. Distinct from `enabled` (`EnableCallbacks`), which gates whether
    /// the plugin processes the input at all.
    array_callbacks: ArrayCallbacks,
    /// Cumulative count of dropped *input* arrays: a full queue or a
    /// compressed array on a non-aware plugin. NOT a MinCallbackTime throttle,
    /// which C counts nowhere. Shared with every upstream sender and with the
    /// admission gate, which is where both drops are decided.
    dropped_arrays: Arc<std::sync::atomic::AtomicI32>,
    /// Last *input* array, cached for ProcessPlugin re-injection
    /// (C++ `pPrevInputArray_`, G5). Released on `EnableCallbacks=0` (B6).
    prev_input_array: Option<PublishedArray>,
    /// Previous array dimensions, for firing an NDDimensions int32-array
    /// callback when dimensions change (C++ `dimsPrev_`, G8). The port
    /// driver's array read serves the same cell.
    dims_prev: Arc<parking_lot::Mutex<[i32; crate::ndarray::ND_ARRAY_MAX_DIMS]>>,
    /// Source address selected via the NDArrayAddr PV (C++ `NDArrayAddr`, G6).
    nd_array_addr: i32,
    /// NumThreads as the port's write path accepted it.
    num_threads: i32,
    /// The band and stack every callback thread gets.
    threads: PluginThreads,
}

impl SharedProcessorInner {
    /// C `pPrevInputArray_`, set in `beginProcessCallbacks`
    /// (NDPluginDriver.cpp:231-234) — so only an array that got past the
    /// admission gate and is about to be processed is ever cached, and
    /// `ProcessPlugin` re-injects the last PROCESSED input rather than the
    /// last one offered.
    ///
    /// The rule for callers: whichever thread is about to run the work calls
    /// this first. That is the callback thread for a queued frame and the
    /// writer's own thread for `blockingCallbacks`, which is exactly the pair
    /// C reaches `processCallbacks` from (`:419-422` vs `processTask`).
    /// Nothing else may write it — an enqueue-time write would name a frame
    /// still waiting in the queue.
    ///
    /// It also adopts the array's pool as the one the plugin allocates from,
    /// C's `this->pNDArrayPool = pArray->pNDArrayPool` (NDPluginDriver.cpp:419),
    /// so a plugin's outputs are counted on the driver whose frame they came
    /// from.
    ///
    /// It counts the frame and returns the rest of C's sets there, which
    /// [`Self::post_process`] stores and `getAttributes` reads before then.
    fn cache_input_array(&mut self, array: &PublishedArray) -> (Arc<NDArrayPool>, BeginSets) {
        self.prev_input_array = Some(array.clone());
        let pool = self.delivery.adopt_pool(array.pool());
        self.array_counter += 1;
        let begin = self.begin_sets(array);
        (pool, begin)
    }

    /// C `readFileBase`'s `beginProcessCallbacks` and `endProcessCallbacks
    /// (pArray, false, true)` for the array ReadFile publishes
    /// (NDPluginFile.cpp:187-193): count it and, when callbacks are on, the
    /// set it gets.
    fn begin_published(
        &mut self,
        array: &NDArray,
        callbacks: bool,
    ) -> (BeginSets, Option<NDAttributeList>) {
        self.array_counter += 1;
        let begin = self.begin_sets(array);
        let attributes = callbacks
            .then(|| self.attributes.evaluate_after(&begin, &[]))
            .flatten();
        (begin, attributes)
    }

    /// `beginProcessCallbacks` and `endProcessCallbacks` for what a write
    /// publishes.
    fn end_published(&mut self, batch: ParamBatch, published: Published) -> ProcessOutput {
        let deliver = self.array_callbacks.get();
        let (arrays, begin, attributes) = match published {
            Published::Nothing => (Vec::new(), None, None),
            Published::Read(array) => {
                let (begin, attributes) = self.begin_published(&array, deliver);
                (vec![Arc::clone(&array)], Some((array, begin)), attributes)
            }
        };
        let ready = self.delivery.end_written(arrays, attributes.as_ref());
        let mut output = self.build_publish_batch(
            batch,
            ready,
            Vec::new(),
            begin
                .as_ref()
                .map(|(array, sets)| (&**array, sets.as_slice())),
            None,
            deliver,
        );
        self.push_status_params(&mut output.batch.sets);
        output
    }

    /// The NDDimensions `doCallbacksInt32Array` of `beginProcessCallbacks`
    /// (NDPluginDriver.cpp:246), when `array`'s dims differ from the
    /// previous input's.
    fn dimensions(&mut self, array: &NDArray) -> Option<asyn_rs::request::ParamSetValue> {
        // G8: fire an int32-array callback on NDDimensions when the array
        // dimensions change (C++ beginProcessCallbacks dimsPrev_). C++ keeps
        // a fixed `dimsPrev_[ND_ARRAY_MAX_DIMS]` zero-filled beyond `ndims`,
        // compares element-wise over all 10 slots, and posts the full
        // 10-element array (NDPluginDriver.cpp:220-231) — so a caget reads
        // NORD=10 with trailing zeros, not `ndims`.
        let mut cur_dims = [0i32; crate::ndarray::ND_ARRAY_MAX_DIMS];
        for (slot, d) in cur_dims
            .iter_mut()
            .zip(array.dims.iter().take(crate::ndarray::ND_ARRAY_MAX_DIMS))
        {
            *slot = d.size as i32;
        }
        let mut dims_prev = self.dims_prev.lock();
        (cur_dims != *dims_prev).then(|| {
            *dims_prev = cur_dims;
            asyn_rs::request::ParamSetValue::array(
                self.ndarray_params.array_dimensions,
                0,
                ParamValue::Int32Array(cur_dims.into()),
            )
        })
    }

    /// C `beginProcessCallbacks`' sets from `array` (NDPluginDriver.cpp:
    /// 216-235), ArrayCounter already counted.
    fn begin_sets(&self, array: &NDArray) -> BeginSets {
        use asyn_rs::param::ParamValue;
        let p = &self.ndarray_params;
        let attr = |name: &str| {
            array
                .attributes
                .get(name)
                .and_then(|a| a.value.as_i64())
                .map(|v| v as i32)
        };
        let color_mode = attr("ColorMode").unwrap_or(array.info().color_mode as i32);
        // An uncompressed array carries an empty codec name and a compressed
        // size equal to its byte count.
        let (codec, compressed) = match &array.codec {
            Some(codec) => (codec.name.octet(), codec.compressed_size as i32),
            None => (
                crate::codec::CodecName::None.octet(),
                array.info().total_bytes as i32,
            ),
        };
        [
            (p.array_counter, 0, ParamValue::Int32(self.array_counter)),
            (
                p.n_dimensions,
                0,
                ParamValue::Int32(array.dims.len() as i32),
            ),
            (
                p.data_type,
                0,
                ParamValue::Int32(array.data().data_type() as i32),
            ),
            (p.color_mode, 0, ParamValue::Int32(color_mode)),
            (
                p.bayer_pattern,
                0,
                ParamValue::Int32(attr("BayerPattern").unwrap_or(0)),
            ),
            (p.codec, 0, ParamValue::Octet(codec)),
            (p.compressed_size, 0, ParamValue::Int32(compressed)),
            (p.unique_id, 0, ParamValue::Int32(array.unique_id)),
            // `setDoubleParam(NDTimeStamp, pArray->timeStamp)`, the driver's
            // own double; NDEpicsTSSec/nSec carry epicsTS.
            (p.timestamp_rbv, 0, ParamValue::Float64(array.time_stamp)),
            (
                p.epics_ts_sec,
                0,
                ParamValue::Int32(array.timestamp.sec as i32),
            ),
            (
                p.epics_ts_nsec,
                0,
                ParamValue::Int32(array.timestamp.nsec as i32),
            ),
        ]
    }

    fn hand_on(&self) -> HandOn {
        HandOn {
            delivery: Arc::clone(&self.delivery),
            attributes: self.attributes.clone(),
            plugin_params: self.plugin_params,
        }
    }

    fn senders_for(&self, output: &ProcessOutput) -> OutputSenders {
        self.delivery.senders(!output.addressed.is_empty())
    }

    /// Process array and return a `ProcessOutput`. Does NOT send to actor.
    /// The returned output must be published and flushed by the caller in async context.
    fn process_and_publish<P: NDPluginProcess>(
        &mut self,
        batch: ParamBatch,
        processor: &P,
        array: &PublishedArray,
    ) -> ProcessOutput {
        let t0 = std::time::Instant::now();
        if !processor.processing_required(array) {
            return self.unprocessed(batch, t0.elapsed().as_secs_f64() * 1000.0);
        }
        let (pool, begin) = self.cache_input_array(array);
        let t0 = std::time::Instant::now();
        let hand_on = self.hand_on();
        let result = run_processor(processor, array.array(), &pool, &begin, Some(&hand_on));
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        self.post_process(batch, array, result, &begin, elapsed_ms)
    }

    /// A frame [`NDPluginProcess::processing_required`] refused: only the
    /// ExecutionTime C sets after `processCallbacks` (NDPluginDriver.cpp:
    /// 438, :537).
    fn unprocessed(&mut self, batch: ParamBatch, elapsed_ms: f64) -> ProcessOutput {
        self.build_publish_batch(batch, Vec::new(), Vec::new(), None, Some(elapsed_ms), false)
    }

    /// The completion half: route the produced arrays and build the param
    /// batch. Runs on the data loop, never on a callback worker, so the
    /// counters keep a single owner however many workers are running; the
    /// sort buffer is [`Delivery`]'s, which a WriteFile hand-on also reaches.
    fn post_process(
        &mut self,
        batch: ParamBatch,
        array: &NDArray,
        result: ProcessResult,
        begin: &[PendingParam],
        elapsed_ms: f64,
    ) -> ProcessOutput {
        let port = &self.port_handle;
        for (mask, line) in &result.trace {
            port.trace().output(port.port_name(), *mask, line);
        }
        // C++ endProcessCallbacks (NDPluginDriver.cpp:257-265): when
        // NDArrayCallbacks==0 the method caches the array and returns BEFORE the
        // throttle / sort-admission / `doCallbacksGenericPointer` path. So a
        // non-delivering frame must not enter the MaxByteRate throttle or the
        // sort buffer — only the metadata params (beginProcessCallbacks) are
        // published. Route (throttle + sort) only when delivering.
        //
        // NDPluginStdArrays is the exception: it sets NDArrayCallbacks=0 yet
        // still serves its typed-array waveforms (STD_ARRAY_DATA). Those are
        // NOT the `doCallbacksGenericPointer` downstream path — they fire
        // regardless of NDArrayCallbacks and ARE subject to the MaxByteRate
        // throttle (NDPluginStdArrays.cpp:58 per-interface `throttled()`). So
        // route whenever we deliver downstream OR serve the StdArray waveforms.
        let produced = result.output_arrays.len();
        // `getAttributes` only past the NDArrayCallbacks return (:289).
        let attributes = (self.array_callbacks.get()
            && self.read_attributes
            && !result.output_arrays.is_empty())
        .then(|| self.attributes.evaluate_after(begin, &result.param_updates))
        .flatten();
        let ready = self.delivery.end_process(
            result.output_arrays,
            attributes.as_ref(),
            CopyArray::IfInput(array),
        );
        // A StdArrays frame that produced a waveform which the MaxByteRate
        // throttle then dropped (`produced > 0` but `ready` empty) must not
        // advance ArrayCounter — C nets it back out
        // (NDPluginStdArrays.cpp:202-211).
        if self.std_array_data_param.is_some() && produced > 0 && ready.is_empty() {
            self.array_counter -= 1;
        }
        if !result.counted {
            self.array_counter -= 1;
        }
        let mut output = self.build_publish_batch(
            batch,
            ready,
            result.param_updates,
            Some((array, begin)),
            Some(elapsed_ms),
            self.array_callbacks.get(),
        );
        output.scatter = result.scatter;
        self.push_status_params(&mut output.batch.sets);
        output.addressed = self.delivery.publishable_addressed(result.addressed);
        output
    }

    /// A param batch carrying only the current DroppedArrays / queue counters,
    /// used when an array is dropped before processing (B5).
    fn dropped_arrays_only_batch(&self) -> ParamBatch {
        let mut batch = ParamBatch::empty();
        self.push_status_params(&mut batch.sets);
        batch
    }

    /// Flush the sort buffer head-first while contiguous or stale (C++
    /// sortingTask periodic tick). Does NOT drain non-contiguous fresh arrays.
    fn tick_sort_buffer(&mut self, batch: ParamBatch) -> ProcessOutput {
        let entries = self.delivery.drain_ready();
        self.emit_drained(batch, entries)
    }

    /// Drain the entire sort buffer in uniqueId order (sort mode turned off).
    fn flush_sort_buffer(&mut self, batch: ParamBatch) -> ProcessOutput {
        let entries = self.delivery.drain_all();
        self.emit_drained(batch, entries)
    }

    fn emit_drained(
        &mut self,
        mut batch: ParamBatch,
        entries: Vec<(i32, Vec<PublishedArray>)>,
    ) -> ProcessOutput {
        let mut all_arrays = Vec::new();
        for (_unique_id, arrays) in entries {
            // Sort-buffer entries were admitted only while NDArrayCallbacks was
            // on (route_output_arrays runs past the delivery gate); the C++
            // sort thread delivers them regardless of the *current* flag, so
            // they always deliver here.
            let output = self.build_publish_batch(batch, arrays, vec![], None, None, true);
            all_arrays.extend(output.arrays);
            batch = output.batch;
        }
        self.push_sort_params(&mut batch.sets);
        ProcessOutput {
            arrays: all_arrays,
            scatter: false,
            batch,
            addressed: vec![],
        }
    }

    fn push_sort_params(&self, sets: &mut Vec<asyn_rs::request::ParamSetValue>) {
        use asyn_rs::request::ParamSetValue;
        let counters = self.delivery.sort_counters();
        sets.extend([
            ParamSetValue::new(
                self.plugin_params.disordered_arrays,
                0,
                ParamValue::Int32(counters.disordered_arrays),
            ),
            ParamSetValue::new(
                self.plugin_params.dropped_output_arrays,
                0,
                ParamValue::Int32(counters.dropped_output_arrays),
            ),
        ]);
        if let Some(free) = counters.sort_free {
            sets.push(ParamSetValue::new(
                self.plugin_params.sort_free,
                0,
                ParamValue::Int32(free),
            ));
        }
    }

    /// Build a param batch carrying the runtime status counters:
    /// DroppedArrays (G1) plus the sort/disorder counters.
    /// Put a control-plane write of a mirrored counter back into the working
    /// copy the data path increments.
    ///
    /// C++ holds all four of these in the param library and increments each
    /// with a read-modify-write — `NDArrayCounter` at
    /// `NDPluginDriver.cpp:206-208`, `DroppedArrays` at `:384-389` and
    /// `:436-441`, `DroppedOutputArrays` read at `:268` and stored at
    /// `:291-292` and `:306-307`, `DisorderedArrays` at `:319-321` and
    /// `:650-652` — so a PV write to any of them resets the count and the next
    /// increment continues from the written value. Rust keeps a working copy
    /// per counter for the data path, which makes the param the source of
    /// truth only if every write lands back here.
    ///
    /// This is the one place that does that, so a mirrored counter cannot be
    /// published without its reset path: adding a counter means adding an arm
    /// here (B12 for `ArrayCounter`, B15 for the other three).
    ///
    /// Returns `true` if `reason` named a mirrored counter. `QueueFree` is
    /// deliberately absent — it is recomputed from the queue depth on every
    /// tick in C and in Rust alike, so a write to it is overwritten rather
    /// than retained.
    fn resync_counter(&mut self, reason: usize, value: i32) -> bool {
        if reason == self.ndarray_params.array_counter {
            self.array_counter = value;
        } else if reason == self.plugin_params.dropped_arrays {
            self.dropped_arrays
                .store(value, std::sync::atomic::Ordering::Release);
        } else if reason == self.plugin_params.dropped_output_arrays {
            self.delivery.resync_dropped_output_arrays(value);
        } else if reason == self.plugin_params.disordered_arrays {
            self.delivery.resync_disordered_arrays(value);
        } else {
            return false;
        }
        true
    }

    fn push_status_params(&self, sets: &mut Vec<asyn_rs::request::ParamSetValue>) {
        use asyn_rs::request::ParamSetValue;
        self.push_sort_params(sets);
        sets.push(ParamSetValue::new(
            self.plugin_params.dropped_arrays,
            0,
            ParamValue::Int32(
                self.dropped_arrays
                    .load(std::sync::atomic::Ordering::Acquire),
            ),
        ));
    }

    /// Build a ProcessOutput: fires direct interrupts (sync) and collects
    /// param updates into a batch. Does NOT publish arrays — the caller
    /// must publish them in async context.
    ///
    /// `deliver` is the NDArrayCallbacks gate (C++ `endProcessCallbacks`,
    /// NDPluginDriver.cpp:257-265): when `false`, the downstream array
    /// delivery — the STD_ARRAY_DATA generic-pointer interrupt and the returned
    /// `ProcessOutput.arrays` — is suppressed, while the `begin` params are
    /// still set.
    ///
    /// `elapsed_ms` is `None` off the input-array path: C sets ExecutionTime
    /// only around `processCallbacks` (NDPluginDriver.cpp:422, :521).
    ///
    /// `begin` is the array a `beginProcessCallbacks` ran on, with its sets
    /// (NDPluginDriver.cpp:207-252): those fields come from it, never from an
    /// output, and only a path that ran it sets them; ArrayCounter takes its
    /// value now, after any frame the plugin did not count.
    fn build_publish_batch(
        &mut self,
        mut batch: ParamBatch,
        output_arrays: Vec<PublishedArray>,
        param_updates: Vec<ParamUpdate>,
        begin: Option<(&NDArray, &[PendingParam])>,
        elapsed_ms: Option<f64>,
        deliver: bool,
    ) -> ProcessOutput {
        use asyn_rs::request::ParamSetValue;

        let sets = &mut batch.sets;
        if let Some((input, begin_sets)) = begin {
            let counter = self.ndarray_params.array_counter;
            sets.extend(
                begin_sets
                    .iter()
                    .filter(|(reason, ..)| *reason != counter)
                    .map(|(reason, addr, value)| ParamSetValue::new(*reason, *addr, value.clone())),
            );
            sets.push(ParamSetValue::new(
                counter,
                0,
                ParamValue::Int32(self.array_counter),
            ));
            // `setTimeStamp(&pArray->epicsTS)` (:232), which the dimensions
            // post and every callback until the next frame carry.
            batch.time_stamp = Some(input.timestamp.to_system_time());
            sets.extend(self.dimensions(input));
        }

        // NDPluginStdArrays' `arrayInterruptCallback`s (NDPluginStdArrays.cpp:
        // 170-197), between the dims post and the frame's params. Not gated by
        // `deliver`: C posts at NDArrayCallbacks 0, StdArrays' default. Only
        // the served output posts, so a throttled frame posts nothing (:58).
        if let (Some(param), Some(served)) = (self.std_array_data_param, output_arrays.first()) {
            let interrupts = self.port_handle.interrupts();
            let data = served.data();
            sets.extend(
                [
                    std_array_post::<i8>(interrupts, param, data),
                    std_array_post::<i16>(interrupts, param, data),
                    std_array_post::<i32>(interrupts, param, data),
                    std_array_post::<i64>(interrupts, param, data),
                    std_array_post::<f32>(interrupts, param, data),
                    std_array_post::<f64>(interrupts, param, data),
                ]
                .into_iter()
                .flatten(),
            );
        }

        // ArrayRate_RBV is computed by a calc record in the DB template
        // (SCAN "1 second", reading ArrayCounter_RBV delta), not in Rust.

        sets.extend(param_updates.iter().map(ParamUpdate::param_set));

        // Set once processCallbacks has returned (NDPluginDriver.cpp:537).
        if let Some(ms) = elapsed_ms {
            sets.push(ParamSetValue::new(
                self.plugin_params.execution_time,
                0,
                ParamValue::Float64(ms),
            ));
        }

        ProcessOutput {
            // NDArrayCallbacks==0 suppresses downstream delivery (C++
            // endProcessCallbacks early-return); the metadata params above are
            // still published.
            arrays: if deliver { output_arrays } else { Vec::new() },
            scatter: false,
            batch,
            addressed: vec![],
        }
    }
}

/// Output from processing: arrays to publish + param batch to flush.
struct ProcessOutput {
    arrays: Vec<PublishedArray>,
    scatter: bool,
    batch: ParamBatch,
    /// `ProcessResult::addressed`, published before `arrays`.
    addressed: Vec<(usize, PublishedArray)>,
}

/// The downstream senders a `ProcessOutput` is published to, cloned under
/// the shared lock and used after it is released.
struct OutputSenders {
    /// Address 0, where `endProcessCallbacks` delivers.
    main: Vec<NDArraySender>,
    /// Per address; filled only when the output has addressed arrays.
    by_addr: Vec<Vec<NDArraySender>>,
}

impl ProcessOutput {
    /// The only way an output's arrays leave: store the batch, deliver the
    /// arrays, then fire the callbacks — C's order under the port lock
    /// (NDPluginDriver.cpp:332, :539), so whoever the delivery reaches reads
    /// this frame's params from the port.
    ///
    /// Broadcast frames are published to every sender concurrently (independent
    /// backpressure per sender). Scatter frames are routed to a single consumer
    /// via `scatter_publish`, which advances the persistent `scatter_cursor`.
    /// Arrays are published in order — the next array's fan-out starts only
    /// after the previous one completes.
    async fn complete(
        self,
        callbacks_gate: &mut CallbacksGate,
        senders: &OutputSenders,
        scatter_cursor: &mut usize,
    ) {
        let callbacks = self.batch.store(callbacks_gate);
        for (addr, arr) in &self.addressed {
            if let Some(subscribers) = senders.by_addr.get(*addr) {
                let futs = subscribers.iter().map(|s| s.publish(arr.clone()));
                futures_util::future::join_all(futs).await;
            }
        }
        let main = senders.main.as_slice();
        for arr in &self.arrays {
            if self.scatter {
                Self::scatter_publish(arr, main, scatter_cursor).await;
            } else {
                let futs = main.iter().map(|s| s.publish(arr.clone()));
                futures_util::future::join_all(futs).await;
            }
        }
        if let Some(callbacks) = callbacks {
            flush_failed(callbacks.end());
        }
    }

    /// Deliver one array to the next downstream consumer in round-robin order,
    /// rerouting past full queues — a port of C++
    /// `NDPluginScatter::doNDArrayCallbacks` (NDPluginScatter.cpp:59-90).
    ///
    /// `cursor` is the persistent `nextClient_`: it advances by one per
    /// *attempt*, so a frame that reroutes past a full consumer leaves the
    /// cursor pointing just past the consumer it actually delivered to (not
    /// merely one past the starting point). Walking begins at `cursor % n` and
    /// makes at most `n` attempts. A full (or disabled/closed) consumer is
    /// rerouted past unless this is the last attempt; only the last node is
    /// allowed to drop the array (C++ sets `auxStatus=asynSuccess` for the last
    /// node so its full queue drops rather than reroutes). Earlier full
    /// consumers are passed `is_last=false` so the rerouted-away drop is *not*
    /// counted (C++ `ignoreQueueFull`, NDPluginDriver.cpp:406,433-442).
    ///
    /// Routing is over the *enabled* senders only: a downstream with callbacks
    /// disabled is unregistered from the interrupt list in C
    /// (`setArrayInterrupt(0)`) and is therefore not a scatter target, so it
    /// must not consume a round-robin slot.
    async fn scatter_publish(arr: &PublishedArray, senders: &[NDArraySender], cursor: &mut usize) {
        let active: Vec<&NDArraySender> = senders.iter().filter(|s| s.is_enabled()).collect();
        let n = active.len();
        if n == 0 {
            return;
        }
        for attempt in 0..n {
            let target = *cursor % n;
            *cursor = cursor.wrapping_add(1);
            let is_last = attempt == n - 1;
            match active[target].publish_scatter(arr.clone(), is_last).await {
                // Delivered: done. (In blocking mode publish always delivers,
                // so the loop breaks on the first attempt — matching C++ where
                // a blocking scatter calls processCallbacks inline and breaks.)
                PublishOutcome::Delivered => break,
                // Full / disabled / closed: reroute to the next consumer unless
                // this was the last attempt (then the array is dropped — already
                // counted by publish_scatter when is_last).
                PublishOutcome::DroppedQueueFull
                | PublishOutcome::Disabled
                | PublishOutcome::ChannelClosed => {
                    if is_last {
                        break;
                    }
                }
                // C reroutes on `auxStatus` alone (NDPluginScatter.cpp:86-90),
                // which `driverCallback` sets on a refused `trySend` and on
                // nothing else — a compressed or time-throttled array leaves
                // it clear, so the scatter treats the node as served.
                PublishOutcome::DroppedCompressed | PublishOutcome::Throttled => break,
            }
        }
    }
}

/// Collected param updates ready to be flushed to the actor.
/// Produced by `build_publish_batch()`, consumed by async `flush()`.
struct ParamBatch {
    /// In C call order, every address in one request: the actor flushes list
    /// 0 and then each further list in the order the batch first set it.
    sets: Vec<asyn_rs::request::ParamSetValue>,
    /// The port timestamp set ahead of the sets; `None` keeps it.
    time_stamp: Option<std::time::SystemTime>,
}

impl ParamBatch {
    fn empty() -> Self {
        Self {
            sets: Vec::new(),
            time_stamp: None,
        }
    }

    /// An empty batch in the `updates` `gate` hands out, which go back to
    /// it once the actor has run the section they are stored in.
    fn new(gate: &CallbacksGate) -> Self {
        Self {
            sets: gate.updates(),
            time_stamp: None,
        }
    }

    /// Store and fire the callbacks in one request, for a batch no delivery
    /// comes between. Call from async context.
    async fn flush(self, port: &PortHandle) {
        flush_failed(match self.time_stamp {
            Some(ts) => {
                port.set_time_stamp_params_and_notify(0, ts, self.sets)
                    .await
            }
            None if !self.sets.is_empty() => port.set_params_and_notify(0, self.sets).await,
            None => Ok(()),
        });
    }

    /// The sets, queued ahead of a delivery; the callbacks fire when the
    /// returned section ends. `None` for an empty batch, which posts nothing.
    fn store(self, gate: &mut CallbacksGate) -> Option<asyn_rs::port_handle::CallbacksAfter<'_>> {
        if self.sets.is_empty() && self.time_stamp.is_none() {
            return None;
        }
        gate.open(0, self.time_stamp, self.sets)
            .map_err(|e| flush_failed(Err(e)))
            .ok()
    }
}

fn flush_failed(sent: AsynResult<()>) {
    if let Err(e) = sent {
        epics_libcom_rs::stderr_println!("plugin param flush error: {e}");
    }
}

/// The `maxThreads` and initial `numThreads` that C takes as `NDPluginDriver`
/// constructor arguments (NDPluginDriver.cpp:158, :159).
///
/// The port's `*Configure` entry points take neither, so the defaults
/// stand in: `maxThreads` floored at 1 as C floors it (`:117`) and published
/// to `MaxThreads` (`:158`), `numThreads` starting at 1 (`:159`). Both are
/// writable at run time, and a `NumThreads` or `QueueSize` write respawns the
/// callback-thread pool exactly as C does (`:730-733`). They are named rather
/// than inlined so the constructor's param block and `SharedProcessorInner`'s
/// initial state cannot drift — publishing one value to the RBV and running on
/// another is what a reader of MaxThreads_RBV would have no way to detect.
const PLUGIN_MAX_THREADS: i32 = 1;
const PLUGIN_NUM_THREADS: i32 = 1;

/// What C `createCallbackThreads` (NDPluginDriver.cpp:955-1024) settles on
/// from the params as they stand, and the lines it prints doing so. Any line
/// means it returned `asynError`.
struct CallbackThreads {
    num_threads: i32,
    queue_size: i32,
    lines: Vec<String>,
}

fn create_callback_threads(num_threads: i32, max_threads: i32, queue_size: i32) -> CallbackThreads {
    let error = "NDPluginDriver::createCallbackThreads error";
    let mut settled = CallbackThreads {
        num_threads,
        queue_size,
        lines: Vec::new(),
    };
    if settled.num_threads > max_threads {
        settled.lines.push(format!(
            "{error}, numThreads={} must be <= maxThreads={max_threads}, setting to {max_threads}\n",
            settled.num_threads
        ));
        settled.num_threads = max_threads;
    }
    if settled.num_threads < 1 {
        settled.lines.push(format!(
            "{error}, numThreads={} must be >= 1, setting to 1\n",
            settled.num_threads
        ));
        settled.num_threads = 1;
    }
    if queue_size < 1 {
        settled.lines.push(format!(
            "{error}, queueSize={queue_size} must be >= 1, setting to 1\n"
        ));
        settled.queue_size = 1;
    }
    settled
}

/// The `NDArrayPort` and `NDArrayAddr` constructor arguments: the source C
/// seeds both params with (NDPluginDriver.cpp:152-153) and connects to.
/// A bare port name is address 0.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArraySource {
    pub port: String,
    pub addr: i32,
}

impl From<&str> for ArraySource {
    fn from(port: &str) -> Self {
        Self {
            port: port.to_string(),
            addr: 0,
        }
    }
}

impl From<&String> for ArraySource {
    fn from(port: &String) -> Self {
        port.as_str().into()
    }
}

impl From<&ArraySource> for ArraySource {
    fn from(source: &ArraySource) -> Self {
        source.clone()
    }
}

impl From<(&str, i32)> for ArraySource {
    fn from((port, addr): (&str, i32)) -> Self {
        Self {
            port: port.to_string(),
            addr,
        }
    }
}

/// asyn's `0`, `ASYN_CANBLOCK` and `ASYN_MULTIDEVICE` (asynDriver.h), for
/// [`NDPluginProcess::ASYN_FLAGS`].
pub const ASYN_NONE: PortFlags = PortFlags {
    multi_device: false,
    can_block: false,
    destructible: false,
};
pub const ASYN_CANBLOCK: PortFlags = PortFlags {
    can_block: true,
    ..ASYN_NONE
};
pub const ASYN_MULTIDEVICE: PortFlags = PortFlags {
    multi_device: true,
    ..ASYN_NONE
};

/// PortDriver implementation for a plugin's control plane.
#[allow(dead_code)]
pub struct PluginPortDriver {
    base: PortDriverBase,
    ndarray_params: NDArrayDriverParams,
    plugin_params: PluginBaseParams,
    /// C `FIRST_NDPLUGIN_PARAM`: the reasons below it are the base class's.
    first_plugin_param: usize,
    /// The plugin's own first reason; its override hands those below to
    /// NDPluginDriver.
    first_processor_param: usize,
    /// `NDPluginProcess::WRITE_INT32_EPILOGUE` / `WRITE_FLOAT64_EPILOGUE`.
    int32_epilogue: WriteEpilogue,
    float64_epilogue: WriteEpilogue,
    param_change_tx: tokio::sync::mpsc::UnboundedSender<PluginParamMsg>,
    /// Optional handle to the latest NDArray for array read methods (used by StdArrays).
    array_data: Option<Arc<parking_lot::Mutex<Option<Arc<NDArray>>>>>,
    /// Param index for STD_ARRAY_DATA (triggers I/O Intr on ArrayData waveform).
    std_array_data_param: Option<usize>,
    /// Where an NDArrayPort/NDArrayAddr write is checked to resolve.
    wiring: Arc<WiringRegistry>,
    /// The plugin, for the checks its C `writeInt32`/`writeFloat64` override
    /// makes before the store.
    validator: Arc<dyn ValidateWrite>,
    /// Where a write hands a frame on, set once the runtime is built.
    hand_on: Arc<std::sync::OnceLock<HandOn>>,
    /// What the plugin's own pool and queued count lead back to.
    owner: Arc<crate::driver::ndarray_driver::ArrayOwner>,
    /// C `pNDArrayPool`, for the base class's pool branch.
    pool: PluginPool,
    /// C `dimsPrev_`, shared with the data side that updates it.
    dims_prev: Arc<parking_lot::Mutex<[i32; crate::ndarray::ND_ARRAY_MAX_DIMS]>>,
    /// C `pThreads_.size() != 0`: whether `createCallbackThreads` has run.
    /// A plugin built with blocking callbacks has not, so its configure-line
    /// queueSize stands unfloored until it first goes non-blocking.
    callback_threads: bool,
}

impl PluginPortDriver {
    /// The served frame converted into `buf`, or 0 elements with none.
    fn read_served<T: StdArrayElement>(&self, buf: &mut [T]) -> usize {
        self.array_data
            .as_ref()
            .and_then(|handle| handle.lock().as_ref().map(|a| T::convert(a.data(), buf)))
            .unwrap_or(0)
    }

    fn new<P: NDPluginProcess>(
        port_name: &str,
        plugin_type_name: &str,
        queue_size: i32,
        blocking_callbacks: i32,
        source: &ArraySource,
        param_change_tx: tokio::sync::mpsc::UnboundedSender<PluginParamMsg>,
        mut processor: P,
        array_data: Option<Arc<parking_lot::Mutex<Option<Arc<NDArray>>>>>,
        max_memory: usize,
        wiring: Arc<WiringRegistry>,
        max_threads: i32,
    ) -> AsynResult<(Self, Arc<P>, PluginPool)> {
        let mut base = PortDriverBase::new(
            port_name,
            processor.max_addr(),
            P::ASYN_FLAGS,
            P::INTERFACE_MASK | PLUGIN_DRIVER_INTERFACES | ND_ARRAY_DRIVER_INTERFACES,
            P::INTERRUPT_MASK | PLUGIN_DRIVER_INTERRUPTS | ND_ARRAY_DRIVER_INTERRUPTS,
        );

        let (ndarray_params, owner) = NDArrayDriverParams::create(&mut base)?;
        // C `asynNDArrayDriver` builds every plugin its own pool,
        // `new NDArrayPool(this, maxMemory)` (asynNDArrayDriver.cpp:881), so
        // its refusals report on the plugin's port.
        let pool = DriverPool::for_driver(max_memory, &owner);
        let first_plugin_param = base.params.len();
        let plugin_params = PluginBaseParams::create(&mut base)?;

        // C++ `NDPluginDriver::NDPluginDriver` (NDPluginDriver.cpp:152-160)
        // initialises exactly these nine read-only / read-back params here,
        // under a comment that states the mechanism: "If a value is not set
        // here then the read request will return an error (uninitialized)".
        // `PluginPortDriver` does not override `read_int32`, so an unset one
        // reaches `get_int32_strict` (asyn-rs/src/port.rs:1663-1665) and the
        // record it feeds sits UDF/INVALID until something else happens to
        // write it — which for MaxThreads_RBV (SCAN "I/O Intr", no PINI, no
        // output partner) is never.
        base.set_string_param(plugin_params.nd_array_port, 0, &source.port)?;
        base.set_int32_param(plugin_params.nd_array_addr, 0, source.addr)?;
        base.set_int32_param(plugin_params.dropped_arrays, 0, 0)?;
        base.set_int32_param(plugin_params.dropped_output_arrays, 0, 0)?;
        // Unfloored: the floor is `createCallbackThreads`' (`:989-995`).
        base.set_int32_param(plugin_params.queue_size, 0, queue_size)?;
        // C `:157` — an empty queue has every slot free. The param is spelled
        // QUEUE_FREE (params.rs:40) whatever the struct field is called, so 0
        // here published a permanently full queue on an idle plugin.
        base.set_int32_param(plugin_params.queue_use, 0, queue_size)?;
        base.set_int32_param(plugin_params.max_threads, 0, max_threads)?;
        base.set_int32_param(plugin_params.num_threads, 0, PLUGIN_NUM_THREADS)?;
        base.set_int32_param(plugin_params.blocking_callbacks, 0, blocking_callbacks)?;
        // C `:164-166`: only a non-blocking plugin starts its threads here.
        let callback_threads = blocking_callbacks == 0;
        if callback_threads {
            let settled = create_callback_threads(PLUGIN_NUM_THREADS, max_threads, queue_size);
            for line in &settled.lines {
                base.trace_print(TraceMask::ERROR, format_args!("{line}"));
            }
            base.set_int32_param(plugin_params.queue_size, 0, settled.queue_size)?;
            base.set_int32_param(plugin_params.queue_use, 0, settled.queue_size)?;
        }

        base.set_string_param(plugin_params.plugin_type, 0, plugin_type_name)?;

        // C++ `NDPluginDriver` derives from `asynNDArrayDriver`, so the base
        // constructor's read-only block (asynNDArrayDriver.cpp:954-1005) runs
        // for every plugin as well.
        crate::driver::ndarray_driver::init_read_only_params(
            &mut base,
            &ndarray_params,
            &pool,
            port_name,
        )?;
        if let Some(seed) = processor.array_callbacks_seed() {
            base.set_int32_param(ndarray_params.array_callbacks, 0, seed as i32)?;
        }
        // NDDimensions posts only when the dims change (NDPluginDriver.cpp:
        // 245-246), so a first frame before the gate left Dimensions_RBV
        // empty until they changed.
        base.set_state_array_rule(ndarray_params.array_dimensions, Arc::new(|| true));
        // Create STD_ARRAY_DATA param for StdArrays plugins (triggers I/O Intr on ArrayData waveform)
        let std_array_data_param = if array_data.is_some() {
            Some(base.create_param("STD_ARRAY_DATA", asyn_rs::param::ParamType::GenericPointer)?)
        } else {
            None
        };

        // Let the processor register its plugin-specific params
        let first_processor_param = base.params.len();
        processor.register_params(&mut base)?;
        processor.bind_attributes(PluginAttributes(Arc::clone(&owner)));
        let pool = PluginPool::new(pool);
        processor.bind_pool(pool.clone());

        let processor = Arc::new(processor);
        let driver = Self {
            base,
            ndarray_params,
            plugin_params,
            first_plugin_param,
            first_processor_param,
            int32_epilogue: P::WRITE_INT32_EPILOGUE,
            float64_epilogue: P::WRITE_FLOAT64_EPILOGUE,
            param_change_tx,
            array_data,
            std_array_data_param,
            wiring,
            validator: processor.clone(),
            hand_on: Arc::default(),
            owner,
            pool: pool.clone(),
            dims_prev: Arc::default(),
            callback_threads,
        };
        Ok((driver, processor, pool))
    }

    fn is_own(&self, reason: usize) -> bool {
        reason >= self.first_processor_param
    }

    /// What C `NDPluginDriver::writeInt32` leaves for an int32 write, which
    /// the plugin's override (`validate_int32`) then sees. For NumThreads
    /// and QueueSize C stores the written value and lets
    /// `createCallbackThreads` overwrite it with the clamp
    /// (NDPluginDriver.cpp:973-994), so the write fails and the readback
    /// shows what runs. C says why on the console and leaves errorMessage
    /// empty (:766-769), whatever the override then makes of the status.
    fn base_int32(&self, reason: usize, value: i32) -> AsynResult<WriteOutcome<i32>> {
        let p = &self.plugin_params;
        // connectToArrayPort reads list 0 (NDPluginDriver.cpp:590-591).
        let port =
            String::from_utf8_lossy(self.base.get_string_param(p.nd_array_port, 0)?).into_owned();
        let (keep, why) = if reason == p.num_threads || reason == p.queue_size {
            // C stores the write, then runs createCallbackThreads on all three
            // params (:746-748), so either write can clamp the other field.
            let get = |r| self.base.get_int32_param(r, 0);
            let (num, queue) = if reason == p.num_threads {
                (value, get(p.queue_size)?)
            } else {
                (get(p.num_threads)?, value)
            };
            let settled = create_callback_threads(num, get(p.max_threads)?, queue);
            if settled.lines.is_empty() {
                return Ok(WriteVerdict::Accept(value).into());
            }
            let keep = if reason == p.num_threads {
                settled.num_threads
            } else {
                settled.queue_size
            };
            (keep, settled.lines)
        } else if reason == p.nd_array_addr {
            match self.unresolved(&port, value) {
                Some(why) => (value, vec![why]),
                None => return Ok(WriteVerdict::Accept(value).into()),
            }
        } else {
            return Ok(WriteVerdict::Accept(value).into());
        };
        let connected = reason != p.nd_array_addr
            && !port.is_empty()
            && self
                .unresolved(&port, self.base.get_int32_param(p.nd_array_addr, 0)?)
                .is_none();
        let name = self.base.params.param_name(reason).unwrap_or("");
        let refused = WriteOutcome::from(WriteVerdict::Refuse {
            keep,
            message: String::new(),
        });
        let refused = why
            .into_iter()
            .fold(refused, |out, line| out.print(TraceMask::ERROR, line));
        Ok(refused.print(
            TraceMask::ERROR,
            format!(
                "NDPluginDriver::writeInt32 ERROR, status=3, function={reason}, paramName={name}, value={value}, connectedToArrayPort_={}\n",
                connected as i32
            ),
        ))
    }

    /// What C `NDPluginDriver::writeOctet` leaves for an octet write, which
    /// the plugin's override (`validate_octet`) then sees.
    ///
    /// An NDArrayPort that `connectToArrayPort` cannot connect to fails the
    /// write, as NDArrayAddr's does (NDPluginDriver.cpp:741): C drops that
    /// status in writeOctet (:836), so the write succeeds; not copied.
    ///
    /// FilePath is normalized by `checkPath`, created to CreateDir's depth
    /// when missing, and FilePathExists set (asynNDArrayDriver.cpp:529-538).
    /// A path still missing fails the write: C takes that status from
    /// `checkPath` (:536) and then overwrites it with callParamCallbacks' at
    /// :540 and again at NDPluginDriver.cpp:845, so the write succeeds; not
    /// copied.
    fn base_octet(&self, reason: usize, value: &str) -> AsynResult<WriteOutcome<String>> {
        let refused = |keep: String| {
            let name = self.base.params.param_name(reason).unwrap_or("");
            WriteVerdict::Refuse {
                keep,
                message: format!(
                    "NDPluginDriver::writeOctet: status=3, function={reason}, paramName={name}, value={value}"
                ),
            }
        };
        let p = &self.plugin_params;
        if reason == p.nd_array_port {
            // connectToArrayPort reads list 0 (NDPluginDriver.cpp:591).
            let nd_addr = self.base.get_int32_param(p.nd_array_addr, 0)?;
            return Ok(match self.unresolved(value, nd_addr) {
                Some(line) => {
                    WriteOutcome::from(refused(value.to_string())).print(TraceMask::ERROR, line)
                }
                None => WriteVerdict::Accept(value.to_string()).into(),
            });
        }
        let nd = &self.ndarray_params;
        if reason != nd.file_path || value.is_empty() {
            return Ok(WriteVerdict::Accept(value.to_string()).into());
        }
        let mut path = value.to_string();
        let mut exists = crate::driver::ndarray_driver::check_path_str(&mut path);
        if !exists {
            let depth = self.base.get_int32_param(nd.create_dir, 0).unwrap_or(0);
            let _ =
                crate::driver::ndarray_driver::NDArrayDriverBase::create_file_path(value, depth);
            exists = crate::driver::ndarray_driver::check_path_str(&mut path);
        }
        let verdict = if exists {
            WriteVerdict::Accept(path)
        } else {
            refused(path)
        };
        Ok(WriteOutcome::from(verdict).set_int32(nd.file_path_exists, 0, exists as i32))
    }

    /// Store what a write override set beside the written param, print its
    /// lines, and post `posts`. A side param at any other address waits for
    /// that address's next post, as NDPluginAttribute's Reset leaves
    /// channels 1.. (:122-136); an array posts at once, as
    /// `doCallbacksFloat64Array` does.
    fn store_side(
        &mut self,
        (first, zero): (Option<i32>, bool),
        side: Vec<SideParam>,
        trace: Vec<(TraceMask, String)>,
    ) -> AsynResult<()> {
        for p in side {
            set_side(&mut self.base, p)?;
        }
        for (mask, line) in trace {
            self.base.trace_print(mask, format_args!("{line}"));
        }
        if let Some(addr) = first {
            self.base.call_param_callbacks(addr)?;
        }
        if zero && first != Some(0) {
            self.base.call_param_callbacks(0)?;
        }
        Ok(())
    }

    fn send_published(&self, publish: Published, addressed: Addressed) {
        if let Published::Read(array) = &publish
            && let Some(hand_on) = self.hand_on.get()
        {
            hand_on.delivery.cache_written(array);
        }
        if !matches!(publish, Published::Nothing) || !addressed.0.is_empty() {
            let _ = self
                .param_change_tx
                .send(PluginParamMsg::Publish(publish, addressed.0));
        }
    }

    /// C `connectToArrayPort` fails when `connectDevice` finds no such port,
    /// with this console line (NDPluginDriver.cpp:605-611).
    /// NDPluginDriver.cpp:766-769 for a write that never reached the param
    /// library. C's paramName is unset when parseAsynUser fails; the
    /// reason's name, or none, is printed instead.
    fn int32_error_line(&self, reason: usize, value: i32, status: i32) -> AsynResult<String> {
        let p = &self.plugin_params;
        let port =
            String::from_utf8_lossy(self.base.get_string_param(p.nd_array_port, 0)?).into_owned();
        let nd_addr = self.base.get_int32_param(p.nd_array_addr, 0)?;
        let connected = !port.is_empty() && self.unresolved(&port, nd_addr).is_none();
        let name = self.base.params.param_name(reason).unwrap_or("");
        Ok(format!(
            "NDPluginDriver::writeInt32 ERROR, status={status}, function={reason}, paramName={name}, value={value}, connectedToArrayPort_={}\n",
            connected as i32
        ))
    }

    fn unresolved(&self, port: &str, addr: i32) -> Option<String> {
        connect_refusal(&self.wiring, port, addr)
    }
}

/// The line C `connectToArrayPort` prints when it fails: `connectDevice`
/// finds no such port, or the port has no asynGenericPointer interface
/// (NDPluginDriver.cpp:605-619). Any address of a port that publishes
/// connects, as `locateDevice` allocates one (asynManager.c:571-589).
fn connect_refusal(wiring: &WiringRegistry, port: &str, addr: i32) -> Option<String> {
    if port.is_empty() || wiring.publishes(port) {
        None
    } else if asyn_rs::registry::get_port(port).is_some() {
        Some(format!(
            "NDPluginDriver::connectToPort ERROR: Can't find asynGenericPointer interface on array port {port} address {addr}\n"
        ))
    } else {
        Some(format!(
            "NDPluginDriver::connectToArrayPort Error calling pasynManager->connectDevice to array port {port} address {addr}, status=3, error=asynManager:connectDevice port {port} not found\n"
        ))
    }
}

fn write_refused(message: String) -> AsynError {
    AsynError::Status {
        status: AsynStatus::Error,
        message,
    }
}

/// Copy source slice directly into destination buffer, returning elements copied.
fn copy_direct<T: Copy>(src: &[T], dst: &mut [T]) -> usize {
    let n = src.len().min(dst.len());
    dst[..n].copy_from_slice(&src[..n]);
    n
}

/// Convert and copy source slice into destination buffer element-by-element.
fn copy_convert<S, D>(src: &[S], dst: &mut [D]) -> usize
where
    S: CastToF64 + Copy,
    D: CastFromF64 + Copy,
{
    let n = src.len().min(dst.len());
    for i in 0..n {
        dst[i] = D::cast_from_f64(src[i].cast_to_f64());
    }
    n
}

/// Cast an integer source element to an integer destination element with C
/// cast semantics. C++ `NDArrayPool::convert` (`NDArrayPool.cpp:387`,
/// `convertType`: `*pDataOut++ = (dataTypeOut)(*pDataIn++)`; and `:465`,
/// `convertDim`) performs a plain C cast between integer types. A C cast:
///   - same-width sign change is a bitwise reinterpret
///     (`(epicsInt8)(epicsUInt8)255 == -1`);
///   - narrowing truncates to the low bits, wrapping
///     (`(epicsInt8)(epicsUInt16)300 == 44`);
///   - widening sign/zero-extends exactly.
///
/// Rust's `as` between integer types implements exactly these semantics. The
/// f64 round-trip in [`copy_convert`] does NOT: it saturates on narrowing
/// (`300.0 as i8 == 127`), diverging from C++. So every integer-source ->
/// integer-target NDArray array read must go through this C-cast path, not
/// `copy_convert`.
trait CCastTo<D> {
    fn ccast(self) -> D;
}
macro_rules! impl_ccast {
    ( $src:ty => $( $dst:ty ),+ ) => {
        $(
            impl CCastTo<$dst> for $src {
                #[inline]
                fn ccast(self) -> $dst {
                    self as $dst
                }
            }
        )+
    };
}
impl_ccast!(i8 => i16, i32, i64);
impl_ccast!(u8 => i8, i16, i32, i64);
impl_ccast!(i16 => i8, i32, i64);
impl_ccast!(u16 => i8, i16, i32, i64);
impl_ccast!(i32 => i8, i16, i64);
impl_ccast!(u32 => i8, i16, i32, i64);
impl_ccast!(i64 => i8, i16, i32);
impl_ccast!(u64 => i8, i16, i32, i64);

/// Copy an integer source slice into an integer destination buffer using C
/// cast semantics (see [`CCastTo`]) — truncating on narrowing, never
/// saturating.
fn copy_ccast<S, D>(src: &[S], dst: &mut [D]) -> usize
where
    S: CCastTo<D> + Copy,
    D: Copy,
{
    let n = src.len().min(dst.len());
    for i in 0..n {
        dst[i] = src[i].ccast();
    }
    n
}

/// Helper trait for `as f64` casts (handles lossy conversions like i64/u64).
trait CastToF64 {
    fn cast_to_f64(self) -> f64;
}

impl CastToF64 for i8 {
    fn cast_to_f64(self) -> f64 {
        self as f64
    }
}
impl CastToF64 for u8 {
    fn cast_to_f64(self) -> f64 {
        self as f64
    }
}
impl CastToF64 for i16 {
    fn cast_to_f64(self) -> f64 {
        self as f64
    }
}
impl CastToF64 for u16 {
    fn cast_to_f64(self) -> f64 {
        self as f64
    }
}
impl CastToF64 for i32 {
    fn cast_to_f64(self) -> f64 {
        self as f64
    }
}
impl CastToF64 for u32 {
    fn cast_to_f64(self) -> f64 {
        self as f64
    }
}
impl CastToF64 for i64 {
    fn cast_to_f64(self) -> f64 {
        self as f64
    }
}
impl CastToF64 for u64 {
    fn cast_to_f64(self) -> f64 {
        self as f64
    }
}
impl CastToF64 for f32 {
    fn cast_to_f64(self) -> f64 {
        self as f64
    }
}
impl CastToF64 for f64 {
    fn cast_to_f64(self) -> f64 {
        self
    }
}

/// Helper trait for `as` casts from f64.
trait CastFromF64 {
    fn cast_from_f64(v: f64) -> Self;
}

impl CastFromF64 for i8 {
    fn cast_from_f64(v: f64) -> Self {
        v as i8
    }
}
impl CastFromF64 for i16 {
    fn cast_from_f64(v: f64) -> Self {
        v as i16
    }
}
impl CastFromF64 for i32 {
    fn cast_from_f64(v: f64) -> Self {
        v as i32
    }
}
impl CastFromF64 for i64 {
    fn cast_from_f64(v: f64) -> Self {
        v as i64
    }
}
impl CastFromF64 for f32 {
    fn cast_from_f64(v: f64) -> Self {
        v as f32
    }
}
impl CastFromF64 for f64 {
    fn cast_from_f64(v: f64) -> Self {
        v
    }
}

/// The element of one asyn array interface, and C `NDArrayPool::convert` of
/// a frame into it. The polled `readXxxArray` and the StdArrays post both
/// convert through it, so a waveform reads one value either way.
trait StdArrayElement: Copy + Default {
    const IFACE: asyn_rs::interfaces::InterfaceType;
    /// Converts `data` into `buf`, returning the elements copied.
    fn convert(data: &crate::ndarray::NDDataBuffer, buf: &mut [Self]) -> usize;
    fn param(elements: Vec<Self>) -> ParamValue;
}

macro_rules! impl_std_array_element {
    (
        $t:ty, $array:ident, $direct_variant:ident,
        ccast: [ $( $ccast_variant:ident ),* ],
        convert: [ $( $variant:ident ),* ]
    ) => {
        impl StdArrayElement for $t {
            const IFACE: asyn_rs::interfaces::InterfaceType =
                asyn_rs::interfaces::InterfaceType::$array;
            fn convert(data: &crate::ndarray::NDDataBuffer, buf: &mut [Self]) -> usize {
                use crate::ndarray::NDDataView;
                match data.view() {
                    NDDataView::$direct_variant(v) => copy_direct(v, buf),
                    $( NDDataView::$ccast_variant(v) => copy_ccast(v, buf), )*
                    $( NDDataView::$variant(v) => copy_convert(v, buf), )*
                }
            }
            fn param(elements: Vec<Self>) -> ParamValue {
                ParamValue::$array(elements.into())
            }
        }
    };
}

// Every integer source -> integer is a C cast (truncating, per C++
// NDArrayPool.cpp:387); float sources keep the numeric f64 conversion.
impl_std_array_element!(
    i8, Int8Array, I8,
    ccast: [U8, I16, U16, I32, U32, I64, U64],
    convert: [F32, F64]
);
impl_std_array_element!(
    i16, Int16Array, I16,
    ccast: [I8, U8, U16, I32, U32, I64, U64],
    convert: [F32, F64]
);
impl_std_array_element!(
    i32, Int32Array, I32,
    ccast: [I8, U8, I16, U16, U32, I64, U64],
    convert: [F32, F64]
);
impl_std_array_element!(
    i64, Int64Array, I64,
    ccast: [I8, U8, I16, U16, I32, U32, U64],
    convert: [F32, F64]
);
impl_std_array_element!(
    f32, Float32Array, F32,
    ccast: [],
    convert: [I8, U8, I16, U16, I32, U32, I64, U64, F64]
);
impl_std_array_element!(
    f64, Float64Array, F64,
    ccast: [],
    convert: [I8, U8, I16, U16, I32, U32, I64, U64, F32]
);

/// One `arrayInterruptCallback` (NDPluginStdArrays.cpp:22-80): `data`
/// converted into `T`'s interface and posted there, only when a client is
/// bound to it — C converts for the first node on the list.
fn std_array_post<T: StdArrayElement>(
    interrupts: &asyn_rs::interrupt::InterruptManager,
    param: usize,
    data: &crate::ndarray::NDDataBuffer,
) -> Option<asyn_rs::request::ParamSetValue> {
    if !interrupts.has_subscriber(param, 0, T::IFACE) {
        return None;
    }
    let mut elements = vec![T::default(); data.len()];
    T::convert(data, &mut elements);
    Some(asyn_rs::request::ParamSetValue::array(
        param,
        0,
        T::param(elements),
    ))
}

impl PortDriver for PluginPortDriver {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn interrupts_opened(&mut self) {
        for post in self.validator.edge_posts() {
            let _ = set_side(&mut self.base, post.into());
        }
    }

    fn io_write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        let reason = user.reason;
        // parseAsynUser (NDPluginDriver.cpp:702); a refusal goes to `done:`.
        let addr = match self.base.parse_user(user) {
            Ok(addr) => addr,
            Err(e) => {
                // getAddress's resolved list, in or out of range.
                let addr = asyn_rs::port::param_list(self.base.flags.multi_device, user.addr);
                let _ = self.base.call_param_callbacks(addr);
                let line = self.int32_error_line(reason, value, e.c_status_code())?;
                self.base
                    .trace_print(TraceMask::ERROR, format_args!("{line}"));
                return Err(e);
            }
        };
        let base = self.base_int32(reason, value)?;
        for (mask, line) in &base.trace {
            self.base.trace_print(*mask, format_args!("{line}"));
        }
        // The thread pool runs what `createCallbackThreads` settled on
        // (`numThreads_`).
        let p = &self.plugin_params;
        let runs = (reason == p.num_threads || reason == p.queue_size).then(|| base.verdict.kept());
        if self.validator.posts_before_write(reason, value) {
            self.base.set_int32_param(reason, addr, value)?;
            self.base.call_param_callbacks(addr)?;
        }
        let WriteOutcome {
            verdict,
            side,
            trace,
            settled,
            publish,
            addressed,
        } = self.validator.int32(
            reason,
            addr,
            value,
            base.verdict.clone(),
            &mut self.base,
            self.hand_on.get(),
        );
        let verdict = verdict.under(base.verdict);
        let value = verdict.kept();
        let own = self.is_own(reason);
        let (at, zero) = self.int32_epilogue.stores(addr, own);
        for a in at.into_iter().chain(zero.then_some(0)) {
            self.base.set_int32_param(reason, a, value)?;
        }
        let base_class = reason < self.first_plugin_param;
        if base_class {
            // NDPluginDriver.cpp:690-692 hands these to asynNDArrayDriver.
            let template = self
                .hand_on
                .get()
                .and_then(|h| h.delivery.last_output_array());
            crate::driver::ndarray_driver::handle_pool_write_int32(
                &mut self.base,
                &self.ndarray_params,
                &self.pool.current(),
                reason,
                template.as_deref(),
            )?;
        }
        let p = self.plugin_params;
        // C runs `createCallbackThreads` on every QueueSize or NumThreads write
        // (NDPluginDriver.cpp:746-748) and on a BlockingCallbacks 0 before any
        // has run (:715-717). That call's status is dropped, so its lines are
        // the only sign; the queue it settles on is the one the data plane was
        // already built with (`ndarray_channel` floors the same value).
        let starts = reason == p.blocking_callbacks && value == 0 && !self.callback_threads;
        if reason == p.queue_size || reason == p.num_threads || starts {
            let settled = create_callback_threads(
                self.base.get_int32_param(p.num_threads, 0)?,
                self.base.get_int32_param(p.max_threads, 0)?,
                self.base.get_int32_param(p.queue_size, 0)?,
            );
            if starts {
                for line in &settled.lines {
                    self.base
                        .trace_print(TraceMask::ERROR, format_args!("{line}"));
                }
            }
            for (other, settled) in [
                (p.num_threads, settled.num_threads),
                (p.queue_size, settled.queue_size),
            ] {
                if other == reason || self.base.get_int32_param(other, 0)? == settled {
                    continue;
                }
                self.base.set_int32_param(other, 0, settled)?;
                if !starts {
                    // A write that clamped the other field: the data plane
                    // runs that one too.
                    let _ = self.param_change_tx.send(PluginParamMsg::Change(
                        other,
                        0,
                        ParamChangeValue::Int32(settled),
                    ));
                }
            }
            // QueueFree = QueueSize (:1024), success or not.
            self.base
                .set_int32_param(p.queue_use, 0, settled.queue_size)?;
            self.callback_threads = true;
        }
        // asynNDArrayDriver.cpp:697 posts list 0 alone, leaving the written
        // address's readback unposted; that C defect is not copied.
        let posts = if base_class {
            (Some(addr), true)
        } else {
            self.int32_epilogue.posts(addr, own)
        };
        self.store_side(posts, side, trace)?;
        self.send_published(publish, addressed);
        if !settled {
            // B14: reliable send on an unbounded channel — never drop param changes.
            let _ = self.param_change_tx.send(PluginParamMsg::Change(
                reason,
                addr,
                ParamChangeValue::Int32(runs.unwrap_or(value)),
            ));
        }
        verdict.into_result()
    }

    fn io_write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        let reason = user.reason;
        // parseAsynUser (NDPluginDriver.cpp:792).
        let addr = self.base.parse_user(user)?;
        let WriteOutcome {
            verdict,
            side,
            trace,
            settled,
            publish,
            addressed,
        } = self
            .validator
            .float64(reason, addr, value, WriteVerdict::Accept(value), &self.base);
        let value = verdict.kept();
        let own = self.is_own(reason);
        let (at, zero) = self.float64_epilogue.stores(addr, own);
        for a in at.into_iter().chain(zero.then_some(0)) {
            self.base.set_float64_param(reason, a, value)?;
        }
        let posts = self.float64_epilogue.posts(addr, own);
        self.store_side(posts, side, trace)?;
        self.send_published(publish, addressed);
        if !settled {
            let _ = self.param_change_tx.send(PluginParamMsg::Change(
                reason,
                addr,
                ParamChangeValue::Float64(value),
            ));
        }
        verdict.into_result()
    }

    fn io_write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        let reason = user.reason;
        // C ignores parseAsynUser's status here (NDPluginDriver.cpp:829) and
        // leaves the address for setStringParam to refuse.
        let addr = self
            .base
            .get_address(user.addr, reason)
            .unwrap_or_else(|_| asyn_rs::port::param_list(self.base.flags.multi_device, user.addr));
        let s = String::from_utf8_lossy(asyn_rs::port::c_str(data));
        let base = self.base_octet(reason, &s)?;
        for (mask, line) in &base.trace {
            self.base.trace_print(*mask, format_args!("{line}"));
        }
        let WriteOutcome {
            verdict,
            mut side,
            trace,
            settled,
            publish,
            addressed,
        } = self
            .validator
            .octet(reason, addr, &s, base.verdict.clone(), &self.base);
        let base_ok = matches!(base.verdict, WriteVerdict::Accept(_));
        let verdict = verdict.under(base.verdict);
        let value = verdict.kept();
        self.base.set_string_param(reason, addr, &value)?;
        // The rest of `asynNDArrayDriver::writeOctet` (NDPluginDriver.cpp:839-841):
        // its FilePath branch is `base_octet`'s. No status is ignored here: the
        // NDAttributesFile/NDAttributesMacros rules already drop
        // readNDAttributesFile's as C does (asynNDArrayDriver.cpp:528), and any
        // other refusal fails the write, which C overwrites at :845; not copied.
        let ruled = match self.base.write_octet_rule(reason) {
            Some(rule) if reason != self.ndarray_params.file_path => {
                rule(&mut self.base, addr, value.as_bytes())
            }
            _ => Ok(()),
        };
        // C sets its errorMessage instead of this line on a failure
        // (asynNDArrayDriver.cpp:542-549).
        if reason < self.first_plugin_param && base_ok && ruled.is_ok() {
            self.base.trace_io_by(
                crate::driver::ndarray_driver::ASYN_ND_ARRAY_DRIVER,
                user,
                "writeOctet",
                format_args!("{s}"),
            );
        }
        side.splice(0..0, base.side);
        self.store_side(WriteEpilogue::Addr.posts(addr, true), side, trace)?;
        self.send_published(publish, addressed);
        if !settled {
            let _ = self.param_change_tx.send(PluginParamMsg::Change(
                reason,
                addr,
                ParamChangeValue::Octet(value),
            ));
        }
        verdict.into_result().and(ruled).map(|()| data.len())
    }

    fn read_int8_array(&mut self, _user: &AsynUser, buf: &mut [i8]) -> AsynResult<usize> {
        Ok(self.read_served(buf))
    }

    fn read_int16_array(&mut self, _user: &AsynUser, buf: &mut [i16]) -> AsynResult<usize> {
        Ok(self.read_served(buf))
    }

    fn read_int32_array(&mut self, user: &AsynUser, buf: &mut [i32]) -> AsynResult<usize> {
        // C `NDPluginDriver::readInt32Array` (NDPluginDriver.cpp:878-882).
        if user.reason == self.ndarray_params.array_dimensions {
            let dims = self.dims_prev.lock();
            let n = dims.len().min(buf.len());
            buf[..n].copy_from_slice(&dims[..n]);
            return Ok(n);
        }
        Ok(self.read_served(buf))
    }

    fn read_int64_array(&mut self, _user: &AsynUser, buf: &mut [i64]) -> AsynResult<usize> {
        Ok(self.read_served(buf))
    }

    fn read_float32_array(&mut self, _user: &AsynUser, buf: &mut [f32]) -> AsynResult<usize> {
        Ok(self.read_served(buf))
    }

    fn read_float64_array(&mut self, _user: &AsynUser, buf: &mut [f64]) -> AsynResult<usize> {
        Ok(self.read_served(buf))
    }
}

/// Handle to a running plugin runtime. Provides access to sender and port handle.
#[derive(Clone)]
pub struct PluginRuntimeHandle {
    port_runtime: PortRuntimeHandle,
    array_sender: NDArraySender,
    array_outputs: Vec<Arc<parking_lot::Mutex<NDArrayOutput>>>,
    port_name: String,
    param_tx: tokio::sync::mpsc::UnboundedSender<PluginParamMsg>,
    pub ndarray_params: NDArrayDriverParams,
    pub plugin_params: PluginBaseParams,
    threads: PluginThreads,
    /// The constructor's source, for [`Self::connect_to_array_port`].
    source: ArraySource,
    /// The plugin's own pool, C `pNDArrayPoolPvt_`.
    own_pool: DriverPool,
}

impl PluginRuntimeHandle {
    /// Subscribe to the constructor's `NDArrayPort`/`NDArrayAddr`, as each C
    /// plugin constructor ends with `connectToArrayPort()` (e.g.
    /// NDPluginROI.cpp:353). Later moves are the data loop's. A port that
    /// does not exist is printed as C prints it, and the plugin stays
    /// unconnected.
    pub fn connect_to_array_port(&self, wiring: &WiringRegistry) -> Result<(), String> {
        let (port, addr) = (&self.source.port, self.source.addr);
        if port.is_empty() {
            return Ok(());
        }
        if let Some(line) = connect_refusal(wiring, port, addr) {
            let handle = self.port_runtime.port_handle();
            handle
                .trace()
                .output(handle.port_name(), TraceMask::ERROR, &line);
            return Ok(());
        }
        wiring.subscribe(&self.array_sender, port, addr)
    }

    /// The pool the plugin was built with, which it allocates from until an
    /// array brings another (C `pNDArrayPoolPvt_`, NDPluginDriver.cpp:419).
    pub fn own_pool(&self) -> &DriverPool {
        &self.own_pool
    }

    /// The band and stack its threads were built with.
    pub fn threads(&self) -> PluginThreads {
        self.threads
    }

    pub fn port_runtime(&self) -> &PortRuntimeHandle {
        &self.port_runtime
    }

    pub fn array_sender(&self) -> &NDArraySender {
        &self.array_sender
    }

    /// The address-0 output, where `endProcessCallbacks` delivers.
    pub fn array_output(&self) -> &Arc<parking_lot::Mutex<NDArrayOutput>> {
        &self.array_outputs[0]
    }

    /// The output of address `addr`, if the port has that address.
    pub fn array_output_at(&self, addr: usize) -> Option<&Arc<parking_lot::Mutex<NDArrayOutput>>> {
        self.array_outputs.get(addr)
    }

    /// Block until the plugin's data thread has applied every control-plane
    /// param change submitted before this call.
    ///
    /// `write_*_blocking` on the port handle returns once the port actor has
    /// recorded the write and queued it for the data plane; the data thread
    /// applies it (the EnableCallbacks flip, NDArrayPort/NDArrayAddr rewiring,
    /// processor param updates) asynchronously. This is the fence between the
    /// two planes: it enqueues a barrier behind every already-queued change
    /// and waits for the data thread to acknowledge it — the param channel is
    /// FIFO, so the ack implies every earlier change is fully applied.
    ///
    /// The ack additionally waits for the array queue to drain, so it also
    /// implies every array published before this call has been fully handled
    /// (processed or throttled). Under continuous array traffic the ack is
    /// therefore delayed until the queue momentarily empties.
    ///
    /// Returns `false` if the data thread has exited or `timeout` elapsed.
    pub fn wait_params_applied(&self, timeout: std::time::Duration) -> bool {
        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(1);
        if self.param_tx.send(PluginParamMsg::Barrier(ack_tx)).is_err() {
            return false;
        }
        ack_rx.recv_timeout(timeout).is_ok()
    }

    pub fn port_name(&self) -> &str {
        &self.port_name
    }
}

/// The stack a plugin's threads get when its `*Configure` gave none.
pub const DEFAULT_PLUGIN_STACK: StackSpec = StackSpec::Class(StackSizeClass::Medium);

/// A `*Configure` command's `stackSize` as the stack its threads get.
///
/// `asynNDArrayDriver.cpp:876-877` turns `stackSize <= 0` into
/// `epicsThreadGetStackSize(epicsThreadStackMedium)` before any thread sees
/// it, so 0 is Medium here, not the Small a bare `epicsThreadCreate` 0 gives;
/// anything else is read as `epicsThreadCreateOpt` reads it.
pub fn plugin_stack(stack_size: i64) -> StackSpec {
    match usize::try_from(stack_size) {
        Ok(n) if n > 0 => StackSpec::from_c(n),
        _ => DEFAULT_PLUGIN_STACK,
    }
}

/// A `*Configure` command's `priority` as the band its threads get.
///
/// `asynNDArrayDriver.cpp:878`: `if (priority <= 0) priority =
/// epicsThreadPriorityMedium`; anything else is the EPICS priority as given.
pub fn plugin_priority(priority: i64) -> ThreadPriority {
    if priority <= 0 {
        ThreadPriority::Medium
    } else {
        ThreadPriority::Custom(u32::try_from(priority).unwrap_or(u32::MAX))
    }
}

/// The band and stack of every thread one plugin creates: C++
/// `threadPriority_` and `threadStackSize_` (`asynNDArrayDriver.cpp:876-879`),
/// which `NDPluginDriver` hands together to each thread
/// (`NDPluginDriver.cpp:1016`, `:1105-1107`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginThreads {
    pub priority: ThreadPriority,
    pub stack: StackSpec,
    /// The `priority` and `stackSize` the plugin's `asynPortDriver` hands
    /// `registerPort` (asynPortDriver.cpp:4031-4036): the `*Configure`
    /// values before `asynNDArrayDriver.cpp:876-879` adjusts them, converted
    /// to `unsigned int` (asynManager.c:2049).
    pub register_priority: u32,
    pub register_stack_size: usize,
    /// The `maxThreads` the plugin runs with: 1 unless the `*Configure` has
    /// one, never below 1 ([`Self::with_max_threads`]).
    max_threads: i32,
}

impl PluginThreads {
    /// These threads, with a `*Configure` command's `maxThreads` floored as
    /// the `NDPluginDriver` constructor floors it (NDPluginDriver.cpp:117).
    pub fn with_max_threads(self, max_threads: i32) -> Self {
        Self {
            max_threads: max_threads.max(1),
            ..self
        }
    }

    /// The port's registration, with C's `registerPort` thread arguments.
    pub fn port_config(&self) -> RuntimeConfig {
        RuntimeConfig {
            priority: self.register_priority,
            stack_size: self.register_stack_size,
            ..RuntimeConfig::default()
        }
    }
}

/// The threads of a plugin whose `*Configure` gave neither.
pub const DEFAULT_PLUGIN_THREADS: PluginThreads = PluginThreads {
    priority: ThreadPriority::Medium,
    stack: DEFAULT_PLUGIN_STACK,
    register_priority: 0,
    register_stack_size: 0,
    max_threads: PLUGIN_MAX_THREADS,
};

/// A `*Configure` command's `priority` and `stackSize`, each read as C reads it.
pub fn plugin_threads(priority: i64, stack_size: i64) -> PluginThreads {
    PluginThreads {
        priority: plugin_priority(priority),
        stack: plugin_stack(stack_size),
        // C's `int` to `unsigned int`: the low 32 bits.
        register_priority: priority as u32,
        register_stack_size: stack_size as u32 as usize,
        max_threads: PLUGIN_MAX_THREADS,
    }
}

/// Create a plugin runtime with control plane (PortActor) and data plane (processing thread),
/// on a port of the processor's [`NDPluginProcess::max_addr`] addresses.
///
/// Returns:
/// - `PluginRuntimeHandle` for wiring and control
/// - `PortRuntimeHandle` for param I/O
/// - `JoinHandle` for the data processing thread
pub fn create_plugin_runtime<P: NDPluginProcess>(
    claim: &asyn_rs::registry::PortClaim,
    processor: P,
    max_memory: usize,
    queue_size: i32,
    blocking_callbacks: i32,
    source: impl Into<ArraySource>,
    wiring: Arc<WiringRegistry>,
    threads: PluginThreads,
) -> (PluginRuntimeHandle, thread::JoinHandle<()>) {
    create_plugin_runtime_with_output(
        claim,
        processor,
        max_memory,
        queue_size,
        blocking_callbacks,
        NDArrayOutput::new(),
        source,
        wiring,
        threads,
    )
}

/// Create a plugin runtime with a pre-wired output at address 0 (for testing
/// and direct wiring).
pub fn create_plugin_runtime_with_output<P: NDPluginProcess>(
    claim: &asyn_rs::registry::PortClaim,
    processor: P,
    max_memory: usize,
    queue_size: i32,
    blocking_callbacks: i32,
    output: NDArrayOutput,
    source: impl Into<ArraySource>,
    wiring: Arc<WiringRegistry>,
    threads: PluginThreads,
) -> (PluginRuntimeHandle, thread::JoinHandle<()>) {
    let source = source.into();
    let port_name = claim.name();
    // Param change channel (control plane -> data plane)
    // B14: unbounded so control-plane param changes (e.g. autosave restoring
    // hundreds of PVs at IOC init) are never silently dropped before the
    // data plane sees them.
    let (param_tx, param_rx) = tokio::sync::mpsc::unbounded_channel::<PluginParamMsg>();
    let handle_param_tx = param_tx.clone();

    // Capture plugin type and array data handle before mutable borrow
    let plugin_type_name = processor.plugin_type().to_string();
    let compression_aware = processor.compression_aware();
    let array_callbacks_seed = processor.array_callbacks_seed();
    // Unseeded, the template's VAL 1 (NDArrayBase.template:758) applies.
    let array_callbacks = ArrayCallbacks::new(array_callbacks_seed.unwrap_or(true));
    let mut processor = processor;
    processor.bind_array_callbacks(array_callbacks.clone());
    let array_data = processor.array_data_handle();

    // Create the port driver for control plane
    let (driver, processor, plugin_pool) = PluginPortDriver::new(
        port_name,
        &plugin_type_name,
        queue_size,
        blocking_callbacks,
        &source,
        param_tx,
        processor,
        array_data,
        max_memory,
        wiring.clone(),
        threads.max_threads,
    )
    .expect("failed to create plugin port driver");
    let own_pool = plugin_pool.current();
    let max_addr = driver.base.max_addr;

    let ndarray_params = driver.ndarray_params;
    let plugin_params = driver.plugin_params;
    let std_array_data_param = driver.std_array_data_param;

    // Create port runtime (actor thread for param I/O).
    //
    // Constructor-shaped, so a failure here is fatal and cannot be anything
    // else: this function hands back the built plugin and has no error channel
    // to its `*Configure` caller. C's equivalent — `asynPortDriver`'s
    // constructor printing and `throw`ing on a failed `registerPort`
    // (asynPortDriver.cpp:4036-4040) — is caught by iocsh
    // (iocsh.cpp:1269-1279) and the script continues by default
    // (iocsh.cpp:995, :1123), leaving the C IOC serving without the port. We
    // deviate on purpose: see `port_runtime_unavailable`.
    let hand_on_cell = Arc::clone(&driver.hand_on);
    let dims_prev = Arc::clone(&driver.dims_prev);
    let (port_runtime, _actor_jh) = create_port_runtime(claim, driver, threads.port_config())
        .unwrap_or_else(|e| port_runtime_unavailable(port_name, &e));

    // Clone port handle for the data thread to write params back
    let port_handle = port_runtime.port_handle().clone();

    // Array channel (data plane)
    let (array_sender, array_rx) = ndarray_channel(port_name, queue_size.max(1) as usize);

    // Shared mode flags
    let enabled = Arc::new(AtomicBool::new(false));
    let blocking_mode = Arc::new(AtomicBool::new(blocking_callbacks != 0));

    // Shared processor (accessible from data thread)
    let array_outputs: Vec<_> = std::iter::once(output)
        .chain((1..max_addr).map(|_| NDArrayOutput::new()))
        .map(|output| Arc::new(parking_lot::Mutex::new(output)))
        .collect();
    // B13/G6: register this plugin's outputs so the WiringRegistry is the
    // single source of truth for runtime rewiring (PluginManager::add_plugin
    // would also register address 0, but direct callers must not bypass the
    // registry). A downstream plugin at NDArrayAddr N gets only what is
    // posted at address N, as on C's per-address interrupt list.
    for (addr, output) in array_outputs.iter().enumerate() {
        wiring.register_output_at(port_name, addr as i32, output.clone());
    }
    // G1/B1: the DroppedArrays counter is owned by this plugin and shared with
    // every upstream sender so full-queue drops on our input queue are counted.
    let dropped_arrays_counter = array_sender.dropped_arrays_counter().clone();
    // The pre-queue gate: C runs both halves of `driverCallback` on the
    // producer's thread before `trySend`, so they live with the sender.
    let admission = array_sender.admission().clone();
    admission.set_compression_aware(compression_aware);
    // The processor is shared with the callback threads rather than owned by
    // `SharedProcessorInner`: the whole point of `&self` on `process_array` is
    // that it runs off the data loop, outside this mutex.
    let delivery = Arc::new(Delivery::new(
        array_outputs.clone(),
        plugin_pool.clone(),
        array_callbacks.clone(),
        std_array_data_param.is_some(),
        port_handle.clone(),
    ));
    let shared = Arc::new(parking_lot::Mutex::new(SharedProcessorInner {
        delivery,
        attributes: PluginAttributes(Arc::clone(own_pool.owner())),
        read_attributes: P::READ_ATTRIBUTES,
        ndarray_params,
        plugin_params,
        port_handle,
        array_counter: 0,
        std_array_data_param,
        array_callbacks,
        dropped_arrays: dropped_arrays_counter,
        prev_input_array: None,
        dims_prev,
        nd_array_addr: source.addr,
        num_threads: PLUGIN_NUM_THREADS,
        threads,
    }));
    let _ = hand_on_cell.set(shared.lock().hand_on());

    let data_enabled = enabled.clone();
    let data_blocking = blocking_mode.clone();

    let mut array_sender = array_sender;
    array_sender.set_mode_flags(enabled, blocking_mode);
    // The data loop owns queue replacement; the handle is weak so it does not
    // keep the channel open past the last real sender.
    let data_queue_handle = array_sender.self_queue_handle();
    let data_sender = array_sender.downgrade();

    // Capture wiring info for data loop
    let sender_port_name = port_name.to_string();
    let initial_upstream = source.clone();

    // Spawn data processing thread
    let data_jh = MandatoryThread::new(
        format!("plugin-data-{port_name}"),
        threads.priority,
        StackSizeClass::Medium,
    )
    .spawn_sized(threads.stack, move || {
        plugin_data_loop(
            shared,
            processor,
            array_rx,
            data_queue_handle,
            data_sender,
            param_rx,
            plugin_params,
            data_enabled,
            data_blocking,
            sender_port_name,
            initial_upstream,
            wiring,
        );
    });

    let handle = PluginRuntimeHandle {
        port_runtime,
        array_sender,
        array_outputs,
        port_name: port_name.to_string(),
        param_tx: handle_param_tx,
        ndarray_params,
        plugin_params,
        threads,
        source,
        own_pool,
    };

    (handle, data_jh)
}

/// Build a param batch reporting the input-queue depth.
///
/// `QUEUE_SIZE` = total capacity, `QUEUE_FREE` = free slots. G2: the param is
/// named `QUEUE_FREE` and the reconciled semantics are *free slots*, matching
/// C++ `NDPluginDriverQueueFree = queueSize - pending()`.
fn queue_status_batch(
    plugin_params: &PluginBaseParams,
    max_capacity: usize,
    free: i32,
) -> ParamBatch {
    use asyn_rs::request::ParamSetValue;
    ParamBatch {
        time_stamp: None,
        sets: vec![
            ParamSetValue::new(
                plugin_params.queue_size,
                0,
                ParamValue::Int32(max_capacity as i32),
            ),
            ParamSetValue::new(plugin_params.queue_use, 0, ParamValue::Int32(free)),
        ],
    }
}

/// One array handed to a callback thread.
///
/// The `ArrayMessage` travels with the work because dropping it is what
/// signals completion to a blocking sender and decrements the queued-array
/// counter — that must happen once the frame has been processed and published,
/// not at dispatch.
struct WorkItem {
    msg: ArrayMessage,
}

/// A processed frame on its way back to the data loop.
struct DoneItem {
    msg: ArrayMessage,
    /// The result and begin sets, `None` for a frame the plugin refused.
    processed: Option<(ProcessResult, BeginSets)>,
    elapsed_ms: f64,
}

/// The port of C's `pToThreadMsgQ_` (NDPluginDriver.cpp:985): one queue, N
/// blocking receivers. `tokio::sync::mpsc` is single-consumer and
/// `crossbeam-channel` is not a dependency of this crate, so this is a plain
/// mutex plus condvar. It is unbounded because the real bound is the plugin's
/// input channel: the data loop dispatches at most `numThreads` frames before
/// it stops receiving, which is what leaves `QueueFree` meaning what C means
/// by it.
struct WorkQueue {
    inner: parking_lot::Mutex<WorkQueueInner>,
    ready: parking_lot::Condvar,
}

struct WorkQueueInner {
    items: std::collections::VecDeque<WorkItem>,
    closed: bool,
}

impl WorkQueue {
    fn new() -> Self {
        Self {
            inner: parking_lot::Mutex::new(WorkQueueInner {
                items: std::collections::VecDeque::new(),
                closed: false,
            }),
            ready: parking_lot::Condvar::new(),
        }
    }

    fn push(&self, item: WorkItem) {
        self.inner.lock().items.push_back(item);
        self.ready.notify_one();
    }

    /// Block until an item is available; `None` once the queue is closed and
    /// drained.
    fn pop(&self) -> Option<WorkItem> {
        let mut guard = self.inner.lock();
        loop {
            if let Some(item) = guard.items.pop_front() {
                return Some(item);
            }
            if guard.closed {
                return None;
            }
            self.ready.wait(&mut guard);
        }
    }

    /// C sends one `ToThreadMessageExit` per thread (NDPluginDriver.cpp:1037);
    /// closing the queue wakes all of them at once.
    fn close(&self) {
        self.inner.lock().closed = true;
        self.ready.notify_all();
    }
}

/// The callback threads of C's `createCallbackThreads`
/// (NDPluginDriver.cpp:996-1001): `numThreads` OS threads all receiving from
/// one queue, so a plugin whose `process_array` is slow overlaps frames
/// instead of serialising them.
struct WorkerPool {
    queue: Arc<WorkQueue>,
    threads: Vec<thread::JoinHandle<()>>,
    size: usize,
}

impl WorkerPool {
    /// How many frames may be in flight at once — the dispatch bound the data
    /// loop applies so undispatched arrays stay counted in `QueueFree`.
    fn size(&self) -> usize {
        self.size
    }

    fn dispatch(&self, msg: ArrayMessage) {
        self.queue.push(WorkItem { msg });
    }

    /// Close the queue and join every worker, C's `deleteCallbackThreads`
    /// (NDPluginDriver.cpp:1015-1074).
    ///
    /// A worker never waits on the data loop — it pushes its `DoneItem` into
    /// an unbounded channel and returns — so joining from the loop cannot
    /// deadlock. Frames finished during the join stay in the done channel for
    /// the caller to drain before it swaps in the new pool.
    ///
    /// Idempotent, because `Drop` runs it again on a pool that was respawned
    /// out of the way.
    fn shutdown(&mut self) {
        self.queue.close();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for WorkerPool {
    /// A dropped pool must not leave its threads parked on the queue forever:
    /// the data loop drops it when the plugin's input channel closes.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Spawn `num_threads` callback threads over one shared queue.
fn spawn_worker_pool<P: NDPluginProcess>(
    port_name: &str,
    shared: Arc<parking_lot::Mutex<SharedProcessorInner>>,
    processor: Arc<P>,
    num_threads: usize,
) -> (WorkerPool, tokio::sync::mpsc::UnboundedReceiver<DoneItem>) {
    let queue = Arc::new(WorkQueue::new());
    let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
    let size = num_threads.max(1);
    let plugin_threads = shared.lock().threads;
    let mut threads = Vec::with_capacity(size);
    for i in 0..size {
        let queue = Arc::clone(&queue);
        let shared = Arc::clone(&shared);
        let processor = Arc::clone(&processor);
        let done_tx = done_tx.clone();
        // C names them `%s_Plugin_%d` from 1 (NDPluginDriver.cpp:997-998) and
        // gives them the driver's stack size and priority (`:1000`).
        let jh = MandatoryThread::new(
            format!("{port_name}_Plugin_{}", i + 1),
            plugin_threads.priority,
            StackSizeClass::Medium,
        )
        .spawn_sized(plugin_threads.stack, move || {
            while let Some(item) = queue.pop() {
                // C's `beginProcessCallbacks` runs here, on the callback
                // thread at the top of `processCallbacks` — so the cached
                // input names the frame being worked on, not the newest one
                // queued behind it.
                let t0 = std::time::Instant::now();
                let processed = processor.processing_required(&item.msg.array).then(|| {
                    let (array_pool, begin, hand_on) = {
                        let mut guard = shared.lock();
                        let (pool, begin) = guard.cache_input_array(&item.msg.array);
                        (pool, begin, guard.hand_on())
                    };
                    let result = run_processor(
                        &*processor,
                        item.msg.array.array(),
                        &array_pool,
                        &begin,
                        Some(&hand_on),
                    );
                    (result, begin)
                });
                let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
                if done_tx
                    .send(DoneItem {
                        msg: item.msg,
                        processed,
                        elapsed_ms,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        threads.push(jh);
    }
    (
        WorkerPool {
            queue,
            threads,
            size,
        },
        done_rx,
    )
}

/// Finish a frame a callback thread has processed: fold the result into the
/// shared state and publish it.
///
/// Everything here runs on the data loop, so the counters keep a single owner
/// however many workers are running — which is what C gets from taking the
/// port lock around `processCallbacks` while the plugin releases it for the
/// time-consuming part (NDPluginDriver.cpp:516-517). The sort buffer and the
/// output senders are [`Delivery`]'s.
async fn complete_frame(
    shared: &Arc<parking_lot::Mutex<SharedProcessorInner>>,
    item: DoneItem,
    callbacks_gate: &mut CallbacksGate,
    scatter_cursor: &mut usize,
) {
    let DoneItem {
        msg,
        processed,
        elapsed_ms,
    } = item;
    let batch = ParamBatch::new(callbacks_gate);
    let (output, senders) = {
        let mut guard = shared.lock();
        let output = match processed {
            Some((result, begin)) => {
                guard.post_process(batch, &msg.array, result, &begin, elapsed_ms)
            }
            None => guard.unprocessed(batch, elapsed_ms),
        };
        let senders = guard.senders_for(&output);
        (output, senders)
    };
    output
        .complete(callbacks_gate, &senders, scatter_cursor)
        .await;
    // msg dropped here → completion signaled (if tracked)
}

/// Move one array-source slot from its `(port, addr)` to another: what
/// `NDPluginDriver::connectToArrayPort` does for its one source
/// (NDPluginDriver.cpp:581-640) and `NDPluginGather::connectToArrayPort` for
/// each of its `maxPorts_` (NDPluginGather.cpp:134-186). As in C the slot is
/// disconnected first, so a source that does not resolve leaves it holding
/// the new pair and receiving nothing.
fn rewire_source(
    wiring: &WiringRegistry,
    sender: &WeakNDArraySender,
    slot: &mut (String, i32),
    new_port: &str,
    new_addr: i32,
) -> Result<(), String> {
    let (old_port, old_addr) = std::mem::replace(slot, (new_port.to_string(), new_addr));
    let sender = sender.upgrade().ok_or("input queue closed")?;
    wiring.unsubscribe(sender.port_name(), &old_port, old_addr);
    if new_port.is_empty() {
        return Ok(());
    }
    wiring.subscribe(&sender, new_port, new_addr)
}

/// The source slot an `NDArrayPort`/`NDArrayAddr` write at `addr` moves, with
/// its index. -1 is asyn's "no address" and reads as 0
/// (asynPortDriver.cpp:1907); an address past the slots moves nothing, as
/// `NDPluginDriver::connectToArrayPort` reads address 0 only.
fn source_slot(sources: &mut [(String, i32)], addr: i32) -> Option<(usize, &mut (String, i32))> {
    let index = usize::try_from(if addr == -1 { 0 } else { addr }).ok()?;
    sources.get_mut(index).map(|slot| (index, slot))
}

fn plugin_data_loop<P: NDPluginProcess>(
    shared: Arc<parking_lot::Mutex<SharedProcessorInner>>,
    processor: Arc<P>,
    mut array_rx: NDArrayReceiver,
    // Lets a QueueSize write replace the queue behind every upstream port's
    // sender. Weak, so holding it cannot stop this loop seeing its own
    // shutdown.
    queue_handle: crate::plugin::channel::SelfQueueHandle,
    // What an `NDArrayPort` write hands the named upstream. Weak for the
    // same reason as `queue_handle`.
    sender: WeakNDArraySender,
    mut param_rx: tokio::sync::mpsc::UnboundedReceiver<PluginParamMsg>,
    plugin_params: PluginBaseParams,
    enabled: Arc<AtomicBool>,
    blocking_mode: Arc<AtomicBool>,
    sender_port_name: String,
    initial_upstream: ArraySource,
    wiring: Arc<WiringRegistry>,
) {
    let enable_callbacks_reason = plugin_params.enable_callbacks;
    let blocking_callbacks_reason = plugin_params.blocking_callbacks;
    let min_callback_time_reason = plugin_params.min_callback_time;
    let sort_mode_reason = plugin_params.sort_mode;
    let sort_time_reason = plugin_params.sort_time;
    let sort_size_reason = plugin_params.sort_size;
    let nd_array_port_reason = plugin_params.nd_array_port;
    let nd_array_addr_reason = plugin_params.nd_array_addr;
    let process_plugin_reason = plugin_params.process_plugin;
    let max_byte_rate_reason = plugin_params.max_byte_rate;
    let num_threads_reason = plugin_params.num_threads;
    let queue_size_reason = plugin_params.queue_size;
    let array_callbacks_reason = shared.lock().ndarray_params.array_callbacks;
    // The array sources: one `(NDArrayPort, NDArrayAddr)` pair per address
    // the processor reads (`num_array_sources`), each wired under its
    // `WiringRegistry::key`. Slot 0 starts on the constructor's port, as
    // NDPluginDriver.cpp:152-153 seeds it.
    let mut sources = vec![(String::new(), 0i32); processor.num_array_sources().max(1)];
    sources[0] = (initial_upstream.port, initial_upstream.addr);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        // Sort flush timer — starts disabled (very long interval).
        // Re-created when sort_time changes.
        let mut sort_flush_interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        let mut sort_flush_active = false;
        // Last published QueueFree value — only flush the queue params when it
        // changes, so a steady queue depth does not spam param callbacks.
        let mut last_queue_free: Option<i32> = None;
        // Persistent scatter cursor (C++ NDPluginScatter::nextClient_): advances
        // per delivery *attempt* across frames so the round-robin survives
        // reroutes past full consumers. One per plugin instance, for its
        // lifetime — matching `nextClient_(1)` set once at construction.
        let mut scatter_cursor: usize = 0;
        // The port's only `CallbacksGate`: every frame's sets and callbacks
        // go through it, one section at a time.
        let mut callbacks_gate = shared.lock().port_handle.callbacks_gate();
        // Barriers held until the array queue drains. A barrier acks only at
        // full quiescence — params applied AND no queued arrays — because a
        // param applied while an older array still waits in the queue would
        // retroactively change that array's processing (e.g. a
        // MinCallbackTime reset un-throttling it). See `PluginParamMsg`.
        let mut held_barriers: Vec<std::sync::mpsc::SyncSender<()>> = Vec::new();
        // The callback threads (C++ `createCallbackThreads`,
        // NDPluginDriver.cpp:996-1001) and the frames currently out with them.
        // `in_flight` is the dispatch bound: an array is left in `array_rx`
        // until a worker is free, so `QueueFree` keeps counting the same thing
        // C counts.
        let initial_threads = shared.lock().num_threads.max(1) as usize;
        let (mut pool, mut done_rx) = spawn_worker_pool(
            &sender_port_name,
            Arc::clone(&shared),
            Arc::clone(&processor),
            initial_threads,
        );
        let mut in_flight: usize = 0;
        // The plugin's admission gate. Held across queue replacements: it
        // belongs to the plugin, not to the queue in front of it, so a
        // QueueSize write must not reset the MinCallbackTime clock.
        let admission = Arc::clone(array_rx.admission());

        loop {
            // Release held barriers once the array queue is empty AND every
            // dispatched frame has come back — a barrier acks at quiescence,
            // and with N workers running an empty input queue no longer implies
            // the last array is finished.
            if !held_barriers.is_empty() && array_rx.pending() == 0 && in_flight == 0 {
                for ack in held_barriers.drain(..) {
                    let _ = ack.try_send(());
                }
            }
            tokio::select! {
                // Stop receiving once every callback thread is busy: the array
                // then stays queued, which is what C's blocking
                // `pToThreadMsgQ_->receive()` does on each of its N threads.
                msg = array_rx.recv_msg(), if in_flight < pool.size() => {
                    match msg {
                        Some(msg) => {
                            // Note there is no `enabled` check here. In C
                            // `EnableCallbacks=0` cancels the array interrupt
                            // (`:711-720` -> `setArrayInterrupt(0)`) and
                            // touches nothing else, so the callback threads go
                            // on emptying `pToThreadMsgQ_` — which is exactly
                            // what `deleteCallbackThreads` relies on when it
                            // disables the interrupt and then waits for
                            // `pending()` to reach zero (`:1026-1034`). The
                            // flag means "is the interrupt registered", and
                            // the producer is the only place that can answer.
                            //
                            // Every array that reaches here already passed both
                            // halves of C's `driverCallback`
                            // (NDPluginDriver.cpp:383-418): the producer ran
                            // them before `trySend`, which is where C runs
                            // them. Nothing is left to classify.
                            let port = shared.lock().port_handle.clone();
                            // G2: update QueueSize/QueueFree from the channel
                            // depth (C++ NDPluginDriver.cpp:512-513). QueueFree
                            // = max_capacity - pending. Only flush when the
                            // value changed to avoid no-op param callbacks.
                            let max_cap = array_rx.max_capacity();
                            let free = max_cap.saturating_sub(array_rx.pending()) as i32;
                            let queue_batch = if last_queue_free != Some(free) {
                                last_queue_free = Some(free);
                                Some(queue_status_batch(&plugin_params, max_cap, free))
                            } else {
                                None
                            };
                            pool.dispatch(msg);
                            in_flight += 1;
                            if let Some(qb) = queue_batch {
                                qb.flush(&port).await;
                            }
                        }
                        None => break,
                    }
                }
                done = done_rx.recv(), if in_flight > 0 => {
                    match done {
                        Some(item) => {
                            in_flight -= 1;
                            complete_frame(&shared, item, &mut callbacks_gate, &mut scatter_cursor)
                                .await;
                        }
                        // Every worker is gone without a reply: nothing can
                        // come back, so stop waiting on frames that are lost.
                        None => in_flight = 0,
                    }
                }
                // A drop the producer counted. C publishes DroppedArrays from
                // `driverCallback` itself (`callParamCallbacks()` at `:449`),
                // on the producer's thread and whether or not the frame was
                // ever processed; here only the data loop may touch the port,
                // so the producer wakes it instead. Without this a run that
                // drops every array — every array compressed on a non-aware
                // plugin, say — would leave the readback reading 0 forever.
                _ = admission.counted_drop() => {
                    let (out, port) = {
                        let guard = shared.lock();
                        (guard.dropped_arrays_only_batch(), guard.port_handle.clone())
                    };
                    out.flush(&port).await;
                }
                // C `driverCallback` republishes QueueFree after each
                // `trySend` (`:447-448`). With a callback thread idle, C's
                // queue hands the array straight to it and the depth does not
                // move; the recv arm above then publishes. Only a backlog
                // behind busy threads shows here.
                _ = admission.depth_moved() => {
                    if in_flight >= pool.size() {
                        let max_cap = array_rx.max_capacity();
                        let free = max_cap.saturating_sub(array_rx.pending()) as i32;
                        if last_queue_free != Some(free) {
                            last_queue_free = Some(free);
                            let port = shared.lock().port_handle.clone();
                            queue_status_batch(&plugin_params, max_cap, free)
                                .flush(&port)
                                .await;
                        }
                    }
                }
                param = param_rx.recv() => {
                    match param {
                        // Barrier: every Change enqueued before it has been
                        // applied by the arms below (FIFO channel). Ack is
                        // deferred to the top-of-loop release, which also
                        // requires the array queue to be drained — a gone
                        // waiter is not an error.
                        Some(PluginParamMsg::Barrier(ack)) => {
                            held_barriers.push(ack);
                        }
                        #[cfg(test)]
                        Some(PluginParamMsg::Fence(ack)) => {
                            let _ = ack.send(());
                        }
                        Some(PluginParamMsg::Publish(published, addressed)) => {
                            let batch = ParamBatch::new(&callbacks_gate);
                            let (po, senders) = {
                                let mut guard = shared.lock();
                                let mut po = guard.end_published(batch, published);
                                po.addressed = guard.delivery.publishable_addressed(addressed);
                                let senders = guard.senders_for(&po);
                                (po, senders)
                            };
                            po.complete(&mut callbacks_gate, &senders, &mut scatter_cursor)
                                .await;
                        }
                        Some(PluginParamMsg::Change(reason, addr, value)) => {
                            if reason == enable_callbacks_reason {
                                let on = value.as_i32() != 0;
                                enabled.store(on, Ordering::Release);
                                // B6: disabling releases the cached input array
                                // (C++ writeInt32 NDPluginDriver.cpp:712-722).
                                if !on {
                                    shared.lock().prev_input_array = None;
                                }
                            }
                            if reason == blocking_callbacks_reason {
                                blocking_mode.store(value.as_i32() != 0, Ordering::Release);
                            }
                            // NDArrayCallbacks gates downstream array delivery
                            // (C++ endProcessCallbacks NDPluginDriver.cpp:
                            // 257-265). Processing still runs; only delivery is
                            // suppressed when 0.
                            if reason == array_callbacks_reason {
                                shared.lock().array_callbacks.set(value.as_i32() != 0);
                            }
                            // Handle MinCallbackTime param change
                            // The gate is on the producer side, where C reads
                            // MinCallbackTime (`:398`).
                            if reason == min_callback_time_reason {
                                admission.set_min_callback_time(value.as_f64());
                            }
                            // G7: MaxByteRate change resets the output throttler
                            // (C++ writeFloat64 NDPluginDriver.cpp:788-790).
                            if reason == max_byte_rate_reason {
                                shared.lock().delivery.set_max_byte_rate(value.as_f64());
                            }
                            if reason == num_threads_reason {
                                shared.lock().num_threads = value.as_i32();
                            }
                            // C `writeInt32` (NDPluginDriver.cpp:730-733): a
                            // NumThreads or QueueSize write deletes the
                            // callback threads and creates them again. A
                            // MaxThreads write that clamps NumThreads downward
                            // lands here too, because the pool is respawned
                            // whenever the accepted NumThreads stops matching
                            // it.
                            //
                            // QueueSize respawns the threads too, because C
                            // creates queue and threads together; the queue
                            // itself was already replaced just above.
                            // C's QueueSize write ends in `new
                            // epicsMessageQueue(queueSize, ...)`
                            // (NDPluginDriver.cpp:730-733 -> :985), so the
                            // depth an operator asks for is the depth they
                            // get. Replace the queue first: publishes racing
                            // this land in one queue or the other and none is
                            // refused. Then drain whatever the old one still
                            // holds, so the resize costs no frames — C loses
                            // any that arrive while it waits for the old queue
                            // to empty with the interrupt switched off
                            // (`deleteCallbackThreads`).
                            if reason == queue_size_reason {
                                let want = value.as_i32().max(1) as usize;
                                let mut old_rx = queue_handle
                                    .replace_queue(want)
                                    .map(|new_rx| std::mem::replace(&mut array_rx, new_rx));
                                while let Some(old) = old_rx.as_mut() {
                                    if in_flight < pool.size() {
                                        if let Some(msg) = old.try_recv_msg() {
                                            // Already through the gate when it
                                            // was queued, and C's resize
                                            // processes the old queue to
                                            // empty with the interrupt already
                                            // off (`:1026-1034`) — so neither
                                            // the gate nor the enable flag is
                                            // consulted again here.
                                            pool.dispatch(msg);
                                            in_flight += 1;
                                            continue;
                                        }
                                    }
                                    if in_flight == 0 {
                                        break;
                                    }
                                    match done_rx.recv().await {
                                        Some(item) => {
                                            in_flight -= 1;
                                            complete_frame(
                                                &shared,
                                                item,
                                                &mut callbacks_gate,
                                                &mut scatter_cursor,
                                            )
                                            .await;
                                        }
                                        None => {
                                            in_flight = 0;
                                            break;
                                        }
                                    }
                                }
                                // C republishes QueueFree at the new depth
                                // (`:1008`).
                                last_queue_free = None;
                            }
                            if reason == num_threads_reason || reason == queue_size_reason {
                                let want = shared.lock().num_threads.max(1) as usize;
                                if reason == queue_size_reason || want != pool.size() {
                                    let (new_pool, new_done) = spawn_worker_pool(
                                        &sender_port_name,
                                        Arc::clone(&shared),
                                        Arc::clone(&processor),
                                        want,
                                    );
                                    let mut old_pool =
                                        std::mem::replace(&mut pool, new_pool);
                                    let mut old_done =
                                        std::mem::replace(&mut done_rx, new_done);
                                    old_pool.shutdown();
                                    // Every worker has exited, so `recv` runs
                                    // out exactly when the last frame the old
                                    // pool finished has been published.
                                    while let Some(item) = old_done.recv().await {
                                        in_flight = in_flight.saturating_sub(1);
                                        complete_frame(
                                            &shared,
                                            item,
                                            &mut callbacks_gate,
                                            &mut scatter_cursor,
                                        )
                                        .await;
                                    }
                                    // C republishes QueueFree after
                                    // createCallbackThreads (`:1006`).
                                    last_queue_free = None;
                                }
                            }
                            // G6: NDArrayAddr selects a source address of a
                            // multi-address driver — reconnect on change
                            // (C++ writeInt32 NDPluginDriver.cpp:724-728).
                            if reason == nd_array_addr_reason {
                                if let Some((index, slot)) = source_slot(&mut sources, addr) {
                                    let port = slot.0.clone();
                                    // A failure was reported by the port write path.
                                    if rewire_source(
                                        &wiring,
                                        &sender,
                                        slot,
                                        &port,
                                        value.as_i32(),
                                    )
                                    .is_ok()
                                        && index == 0
                                    {
                                        shared.lock().nd_array_addr = slot.1;
                                    }
                                }
                            }
                            // G5: ProcessPlugin re-injects the cached input
                            // array (C++ writeInt32 NDPluginDriver.cpp:739-746)
                            // by handing it to `driverCallback` (`:741`) — the
                            // same entry point a detector array arrives
                            // through. So it is not a shortcut past the queue:
                            // it is classified and enqueued like any arrival,
                            // waits behind whatever is already queued, is
                            // refused and counted when the queue is full, and
                            // runs on a callback thread. Only `driverCallback`'s
                            // blocking arm (`:419-422`) runs the work on the
                            // writer's own thread, which is the inline branch
                            // below. Any value triggers it, 0 included (`:755`).
                            if reason == process_plugin_reason {
                                let cached = shared.lock().prev_input_array.clone();
                                let blocking = blocking_mode.load(Ordering::Acquire);
                                match cached {
                                    Some(array) if !blocking => {
                                        // `try_enqueue` runs both gates before
                                        // it touches the queue, exactly where
                                        // `driverCallback` runs them, so a
                                        // re-injection inside the
                                        // MinCallbackTime window takes no queue
                                        // slot and a compressed one on a
                                        // non-aware plugin is counted without
                                        // taking one either.
                                        if let Some(
                                            PublishOutcome::DroppedQueueFull
                                            | PublishOutcome::DroppedCompressed,
                                        ) = queue_handle.try_enqueue(array)
                                        {
                                            // The counter the enqueue bumped is
                                            // the plugin's own; only the
                                            // readback is owed (C `:387-390`
                                            // and `:436-441`, then
                                            // `callParamCallbacks` at `:751`).
                                            let (out, port) = {
                                                let guard = shared.lock();
                                                (
                                                    guard.dropped_arrays_only_batch(),
                                                    guard.port_handle.clone(),
                                                )
                                            };
                                            out.flush(&port).await;
                                        }
                                    }
                                    Some(array) => {
                                        // Blocking mode runs the work on the
                                        // writer's own thread, but C decides
                                        // admission BEFORE it reads
                                        // `blockingCallbacks` (`:385-407` vs
                                        // `:419`), so the same two gates apply
                                        // to the inline arm.
                                        match admission.classify(&array) {
                                            Admission::Throttled => {}
                                            Admission::DropCompressed => {
                                                let (out, port) = {
                                                    let guard = shared.lock();
                                                    guard
                                                        .dropped_arrays
                                                        .fetch_add(1, Ordering::AcqRel);
                                                    (
                                                        guard.dropped_arrays_only_batch(),
                                                        guard.port_handle.clone(),
                                                    )
                                                };
                                                out.flush(&port).await;
                                            }
                                            Admission::Admit => {
                                                let (po, senders) = {
                                                    let mut guard = shared.lock();
                                                    let output = guard.process_and_publish(
                                                        ParamBatch::new(&callbacks_gate),
                                                        &*processor,
                                                        &array,
                                                    );
                                                    let senders = guard.senders_for(&output);
                                                    (output, senders)
                                                };
                                                po.complete(
                                                    &mut callbacks_gate,
                                                    &senders,
                                                    &mut scatter_cursor,
                                                )
                                                .await;
                                            }
                                        }
                                    }
                                    None => {
                                        // C parity: NDPluginDriver::writeInt32
                                        // (NDPluginDriver.cpp:759-761) logs this
                                        // through the writing record's user at
                                        // ASYN_TRACE_WARNING, which is OFF in the
                                        // default port trace mask. Gate through the
                                        // port's trace facility so iocInit stays
                                        // silent unless WARNING is enabled, instead
                                        // of an unconditional eprintln! that spams
                                        // stderr on every PINI ProcessPlugin trigger.
                                        // The asyn port registry that exposes the
                                        // per-port mask only exists with the `ioc`
                                        // integration; a bare plugin build has no
                                        // trace facility to consult, so it stays
                                        // silent there too.
                                        #[cfg(feature = "ioc")]
                                        if let Some(entry) =
                                            asyn_rs::asyn_record::get_port(&sender_port_name)
                                        {
                                            entry.handle.trace().print(
                                                Some(sender_port_name.as_str()),
                                                Some(addr),
                                                reason as i32,
                                                asyn_rs::trace::TraceMask::WARNING,
                                                format_args!(
                                                    "NDPluginDriver::writeInt32 cannot do \
                                                     ProcessPlugin, no input array cached\n"
                                                ),
                                            );
                                        }
                                    }
                                }
                            }
                            // B12/B15: a control-plane write of any mirrored
                            // counter resets it. `resync_counter` owns the whole
                            // family so none can be published without a reset.
                            shared.lock().resync_counter(reason, value.as_i32());
                            // Handle sort param changes
                            if reason == sort_mode_reason {
                                let mode = value.as_i32();
                                // Scope the guard so clippy can verify the lock
                                // is released before any await.
                                let flush_work = {
                                    let mut guard = shared.lock();
                                    let sort_time = guard.delivery.set_sort_mode(mode);
                                    if mode == 0 {
                                        let output = guard
                                            .flush_sort_buffer(ParamBatch::new(&callbacks_gate));
                                        let senders = guard.senders_for(&output);
                                        sort_flush_active = false;
                                        Some((output, senders))
                                    } else {
                                        sort_flush_active = sort_time > 0.0;
                                        if sort_flush_active {
                                            let dur = epics_libcom_rs::runtime::time::duration_from_secs(
                                                sort_time,
                                            );
                                            sort_flush_interval = tokio::time::interval(dur);
                                        }
                                        None
                                    }
                                };
                                if let Some((output, senders)) = flush_work {
                                    output
                                        .complete(&mut callbacks_gate, &senders, &mut scatter_cursor)
                                        .await;
                                }
                            }
                            if reason == sort_time_reason {
                                let t = value.as_f64();
                                let sort_mode = shared.lock().delivery.set_sort_time(t);
                                if sort_mode != 0 && t > 0.0 {
                                    sort_flush_active = true;
                                    let dur = epics_libcom_rs::runtime::time::duration_from_secs(t);
                                    sort_flush_interval = tokio::time::interval(dur);
                                } else {
                                    sort_flush_active = false;
                                }
                            }
                            if reason == sort_size_reason {
                                shared.lock().delivery.set_sort_size(value.as_i32());
                            }
                            // NDArrayPort at address `a` moves source slot `a`
                            // (NDPluginDriver.cpp:834-836).
                            if reason == nd_array_port_reason {
                                if let (Some((_, slot)), Some(new_port)) =
                                    (source_slot(&mut sources, addr), value.as_string())
                                {
                                    let source_addr = slot.1;
                                    // A failure was reported by the port write path.
                                    let _ = rewire_source(
                                        &wiring,
                                        &sender,
                                        slot,
                                        new_port,
                                        source_addr,
                                    );
                                }
                            }
                            let snapshot = PluginParamSnapshot {
                                enable_callbacks: enabled.load(Ordering::Acquire),
                                reason,
                                addr,
                                value,
                            };
                            let (process_output, senders) = {
                                let mut guard = shared.lock();
                                let result = processor.on_param_change(reason, &snapshot);
                                let output = if !result.output_arrays.is_empty()
                                    || !result.param_updates.is_empty()
                                    || !result.addressed.is_empty()
                                {
                                    let deliver = guard.array_callbacks.get();
                                    let arrays = guard.delivery.publishable(result.output_arrays, None, CopyArray::Never);
                                    let mut output = guard.build_publish_batch(ParamBatch::new(&callbacks_gate), arrays, result.param_updates, None, None, deliver);
                                    output.addressed = guard.delivery.publishable_addressed(result.addressed);
                                    Some(output)
                                } else {
                                    None
                                };
                                let senders = output.as_ref().map(|o| guard.senders_for(o));
                                (output, senders)
                            };
                            if let (Some(po), Some(senders)) = (process_output, senders) {
                                po.complete(&mut callbacks_gate, &senders, &mut scatter_cursor)
                                    .await;
                            }
                        }
                        None => break,
                    }
                }
                _ = sort_flush_interval.tick(), if sort_flush_active => {
                    // B3: drain head-first while contiguous or past the
                    // staleness deadline — NOT the whole buffer.
                    let (output, senders) = {
                        let mut guard = shared.lock();
                        let output = guard.tick_sort_buffer(ParamBatch::new(&callbacks_gate));
                        let senders = guard.senders_for(&output);
                        (output, senders)
                    };
                    output
                        .complete(&mut callbacks_gate, &senders, &mut scatter_cursor)
                        .await;
                }
            }
        }
    });
}

/// Connect a downstream plugin's sender to a plugin runtime's output.
///
/// B13: the upstream's `array_output` is the same `Arc` that every
/// `create_plugin_runtime*` entry point registers in the `WiringRegistry`, so
/// adding a sender here mutates the registry-tracked output — the registry
/// remains the single source of truth for `rewire_source`.
pub fn wire_downstream(upstream: &PluginRuntimeHandle, downstream_sender: NDArraySender) {
    upstream.array_output().lock().add(downstream_sender);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ndarray::{NDDataType, NDDimension};
    use crate::plugin::channel::ndarray_channel;

    /// # Invariant
    ///
    /// MUST: every `plugin-data-*` thread and every `<port>_Plugin_<n>`
    /// callback thread be created through [`MandatoryThread`], so that a
    /// thread the plugin cannot process without takes the process down rather
    /// than the caller's thread.
    ///
    /// The reason this is not the `errlog-and-continue` class:
    /// `NDPluginDriver::createCallbackThreads` builds its workers as
    /// `new epicsThread(...)` (`NDPluginDriver.cpp:1000`), whose constructor
    /// calls `epicsThreadCreateOpt` and `throw unableToCreateThread()` on
    /// failure (`epicsThread.cpp:214-220`) — a thrown failure, not a status
    /// code the plugin inspects and carries on from.
    ///
    /// Where C ends up is **not** where we do, and the difference is
    /// deliberate: iocsh catches whatever a command throws
    /// (`iocsh.cpp:1269-1279`, `"C++ error: ..."`) and a startup script's
    /// default `on error` is `Continue` (`iocsh.cpp:995`, `:1123`), so C runs
    /// the rest of st.cmd with the plugin's port registered and its worker
    /// threads absent — arrays queue to it and are never processed, silently,
    /// for the life of the IOC. `MandatoryThread::spawn` refuses that state.
    /// The `.expect` it replaced reached neither: on a `panic = "unwind"`
    /// target it unwound one thread and left the same zombie plugin behind.
    ///
    /// Contrast the auxiliary AD threads, which genuinely do errlog-and-continue
    /// and have no site here: the sorting thread (`NDPluginDriver.cpp:1089-1098`,
    /// `asynPrint` + `return asynError`), the queued-array counter
    /// (`asynNDArrayDriver.cpp:1013-1021`, `asynPrint` and no error at all) and
    /// the HDF5 flush task (`NDFileHDF5.cpp:2421-2429`, `printf` + `return`).
    ///
    /// Source inspection, because the defect is a call that is *absent*.
    #[test]
    fn plugin_data_threads_are_mandatory() {
        let prod =
            source_guard::production(include_str!("runtime.rs"), source_guard::Comments::Strip);
        assert_eq!(
            prod.matches("MandatoryThread::new(").count(),
            2,
            "`create_plugin_runtime_with_output` and `spawn_worker_pool`"
        );
        let bare = concat!("thread", "::Builder::new()");
        let strays: Vec<&str> = prod
            .lines()
            .map(str::trim)
            .filter(|l| l.contains(bare) || l.contains(concat!("thread", "::spawn(")))
            .collect();
        assert!(
            strays.is_empty(),
            "a plugin data thread created outside `MandatoryThread` resolves its \
             own spawn failure locally: {strays:?}"
        );
    }

    /// Passthrough processor: returns the input array as-is.
    struct PassthroughProcessor;

    impl NDPluginProcess for PassthroughProcessor {
        const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
        const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
        fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
            ProcessResult::forward(array, vec![])
        }
        fn plugin_type(&self) -> &str {
            "Passthrough"
        }
    }

    /// Clamps `LIMIT` to 0..=10 and reverts any negative `GAIN`, as C plugin
    /// overrides do; mirrors `LIMIT` into `TWICE`; accepts a base refusal of
    /// 999 as a C override that overwrites the status does, and 998 over the
    /// base clamp as NDPluginCodec does; records what reached the data plane.
    #[derive(Default)]
    struct ValidatingProcessor {
        addrs: usize,
        limit: usize,
        twice: usize,
        gain: usize,
        name: usize,
        threads: usize,
        applied: Arc<parking_lot::Mutex<Vec<usize>>>,
        threads_run: Arc<parking_lot::Mutex<Vec<i32>>>,
        ids: Arc<parking_lot::Mutex<(usize, usize, usize, usize)>>,
    }

    impl NDPluginProcess for ValidatingProcessor {
        const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
        const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
        fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
            ProcessResult::forward(array, vec![])
        }
        fn plugin_type(&self) -> &str {
            "Validating"
        }
        fn max_addr(&self) -> usize {
            self.addrs
        }
        fn register_params(&mut self, base: &mut PortDriverBase) -> Result<(), AsynError> {
            self.limit = base.create_param("LIMIT", asyn_rs::param::ParamType::Int32)?;
            self.twice = base.create_param("TWICE", asyn_rs::param::ParamType::Int32)?;
            self.gain = base.create_param("GAIN", asyn_rs::param::ParamType::Float64)?;
            self.name = base.create_param("NAME", asyn_rs::param::ParamType::Octet)?;
            base.set_float64_param(self.gain, 0, 1.0)?;
            self.threads = base.find_param("NUM_THREADS").unwrap();
            *self.ids.lock() = (self.limit, self.twice, self.gain, self.name);
            Ok(())
        }
        fn validate_int32(
            &self,
            reason: usize,
            addr: i32,
            value: i32,
            verdict: WriteVerdict<i32>,
            _params: &mut WritePort<'_>,
        ) -> WriteOutcome<i32> {
            // Busy, as WriteFile is: the work is done inside the write.
            if reason == self.limit && value == 6 {
                return WriteVerdict::Accept(0).into();
            }
            if reason != self.limit {
                return match value {
                    999 => WriteVerdict::Accept(verdict.kept()).into(),
                    998 => WriteVerdict::Accept(value).into(),
                    _ => verdict.into(),
                };
            }
            let verdict = if value > 10 {
                WriteVerdict::Refuse {
                    keep: 10,
                    message: "limit".into(),
                }
            } else {
                verdict
            };
            let kept = verdict.kept();
            let out = WriteOutcome::from(verdict)
                .set_int32(self.twice, addr, 2 * kept)
                .print(TraceMask::ERROR, "limit set\n");
            match value {
                // Also sets TWICE at the next address, as Attribute's Reset
                // sets every channel.
                7 => out.set_int32(self.twice, addr + 1, 2 * kept),
                // Hands an array downstream, as ReadFile does.
                5 => out.publish_read(raw_test_array(5)),
                _ => out,
            }
        }
        fn posts_before_write(&self, reason: usize, value: i32) -> bool {
            reason == self.limit && value == 6
        }
        fn validate_float64(
            &self,
            reason: usize,
            addr: i32,
            value: f64,
            verdict: WriteVerdict<f64>,
            params: &PortDriverBase,
        ) -> WriteOutcome<f64> {
            if reason == self.gain && value < 0.0 {
                return WriteOutcome::from(WriteVerdict::Refuse {
                    keep: params.get_float64_param(reason, addr).unwrap(),
                    message: "gain".into(),
                })
                .settled();
            }
            verdict.into()
        }
        /// Keeps and fails `bad`, as NDFileHDF5 keeps an XMLFileName it
        /// cannot open; reverts `old`; accepts any other param's write,
        /// dropping the base status as NDFileNexus.cpp:801 does.
        fn validate_octet(
            &self,
            reason: usize,
            addr: i32,
            value: &str,
            verdict: WriteVerdict<String>,
            params: &PortDriverBase,
        ) -> WriteOutcome<String> {
            let refuse = |keep: String| WriteVerdict::Refuse {
                keep,
                message: "name".into(),
            };
            match value {
                "bad" if reason == self.name => refuse(value.into()).into(),
                "old" if reason == self.name => WriteOutcome::from(refuse(
                    String::from_utf8_lossy(params.get_string_param(reason, addr).unwrap())
                        .into_owned(),
                ))
                .settled(),
                _ if reason != self.name => WriteVerdict::Accept(verdict.kept()).into(),
                _ => verdict.into(),
            }
        }
        fn on_param_change(
            &self,
            reason: usize,
            params: &PluginParamSnapshot,
        ) -> ParamChangeResult {
            self.applied.lock().push(reason);
            if reason == self.threads {
                self.threads_run.lock().push(params.value.as_i32());
            }
            ParamChangeResult::empty()
        }
    }

    /// A plugin port registers NDPluginDriver's and asynNDArrayDriver's
    /// interfaces (NDPluginDriver.cpp:91, asynNDArrayDriver.cpp:866): asynRecord
    /// finds no asynOption or asynUInt32Digital there.
    #[test]
    fn a_plugin_port_registers_c_s_interfaces() {
        use asyn_rs::interfaces::InterfaceType;
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("PLUGIN_IFACES").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        assert!(!port.has_interface(InterfaceType::Option));
        assert!(!port.has_interface(InterfaceType::UInt32Digital));
        assert!(port.has_interface(InterfaceType::Int32Array));
        assert!(port.has_interface(InterfaceType::GenericPointer));
    }

    /// A plugin is an asynNDArrayDriver (NDPluginDriver.h:57), so its
    /// AcquireBusy follows its Acquire (asynNDArrayDriver.cpp:636-663).
    #[test]
    fn a_plugin_port_drives_acquire_busy() {
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("PLUGIN_BUSY").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        let (acquire, busy) = (
            handle.ndarray_params.acquire,
            handle.ndarray_params.acquire_busy,
        );
        for value in [1, 0] {
            port.write_int32_blocking(acquire, 0, value).unwrap();
            assert_eq!(port.read_int32_blocking(busy, 0).unwrap(), value);
        }
    }

    /// A base refusal stands, with its clamp, over every override verdict;
    /// an accepted base write ends with the override's.
    #[test]
    fn an_override_cannot_drop_a_base_refusal() {
        let refuse = |keep, message: &str| WriteVerdict::Refuse {
            keep,
            message: message.into(),
        };
        let base = refuse(4, "");
        for own in [WriteVerdict::Accept(4), WriteVerdict::Accept(98)] {
            assert_eq!(own.under(base.clone()), base);
        }
        assert_eq!(refuse(9, "own").under(base), refuse(4, "own"));
        let accepted = WriteVerdict::Accept(5);
        assert_eq!(
            WriteVerdict::Accept(7).under(accepted.clone()),
            WriteVerdict::Accept(7)
        );
        assert_eq!(refuse(9, "own").under(accepted), refuse(9, "own"));
    }

    /// The port asks the plugin before it stores: a clamp is stored and
    /// fails, a revert keeps the old value, fails, and never reaches
    /// `on_param_change`.
    #[test]
    fn plugin_validate_hook_clamps_and_reverts_before_the_store() {
        let applied = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let ids = Arc::new(parking_lot::Mutex::new((0, 0, 0, 0)));
        let threads_run = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("VALIDATE").unwrap(),
            ValidatingProcessor {
                applied: applied.clone(),
                threads_run: threads_run.clone(),
                ids: ids.clone(),
                ..Default::default()
            },
            1_000_000,
            10,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        let (limit, twice, gain, name) = *ids.lock();

        assert!(port.write_int32_blocking(limit, 0, 5).is_ok());
        assert_eq!(port.read_int32_blocking(twice, 0).unwrap(), 10);
        assert!(port.write_int32_blocking(limit, 0, 50).is_err());
        assert_eq!(port.read_int32_blocking(limit, 0).unwrap(), 10);
        assert_eq!(port.read_int32_blocking(twice, 0).unwrap(), 20);

        // The base clamp and failure stand, whether the override keeps the
        // failure, accepts the clamp or accepts the written value.
        let num_threads = handle.plugin_params.num_threads;
        let max = port
            .read_int32_blocking(handle.plugin_params.max_threads, 0)
            .unwrap();
        for written in [50, 999, 998] {
            assert!(
                port.write_int32_blocking(num_threads, 0, written).is_err(),
                "{written}"
            );
            assert_eq!(
                port.read_int32_blocking(num_threads, 0).unwrap(),
                max,
                "{written}"
            );
        }

        assert!(port.write_float64_blocking(gain, 0, 2.0).is_ok());
        assert!(port.write_float64_blocking(gain, 0, -1.0).is_err());
        assert_eq!(port.read_float64_blocking(gain, 0).unwrap(), 2.0);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let write = |v: &str| rt.block_on(port.write_octet(name, 0, v.as_bytes().to_vec()));
        let read = || {
            let (data, status) = rt.block_on(port.read_octet(name, 0, 64)).into_parts();
            status.unwrap();
            data
        };
        // asynPortDriver::readOctet counts the NUL: `strlen(value) + 1`.
        assert!(write("good").is_ok());
        assert!(write("old").is_err());
        assert_eq!(read(), b"good\0");
        assert!(write("bad").is_err());
        assert_eq!(read(), b"bad\0");
        // CreateDir 0 leaves the path missing.
        let file_path = handle.ndarray_params.file_path;
        let missing = std::env::temp_dir().join(format!("validate-{}/a", std::process::id()));
        assert!(
            rt.block_on(port.write_octet(file_path, 0, missing.to_str().unwrap().into()))
                .is_err()
        );

        params_applied(&handle);
        assert_eq!(
            *applied.lock(),
            vec![
                limit,
                limit,
                num_threads,
                num_threads,
                num_threads,
                gain,
                name,
                name,
                file_path
            ]
        );
        // C recreates the callback threads on every NumThreads write, a
        // refused one included (NDPluginDriver.cpp:730-733).
        assert_eq!(*threads_run.lock(), vec![max, max, max]);
    }

    /// A processor that serves its latest frame, as NDPluginStdArrays does.
    struct ServingProcessor {
        served: Arc<parking_lot::Mutex<Option<Arc<NDArray>>>>,
    }

    impl NDPluginProcess for ServingProcessor {
        const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
        const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
        fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
            ProcessResult::forward(array, vec![])
        }
        fn plugin_type(&self) -> &str {
            "Serving"
        }
        fn array_data_handle(&self) -> Option<Arc<parking_lot::Mutex<Option<Arc<NDArray>>>>> {
            Some(Arc::clone(&self.served))
        }
    }

    /// A forwarded frame goes out as the input `Arc` itself — no copy — and
    /// lands in the processor's served-array handle.
    #[test]
    fn a_forwarded_frame_is_the_input_arc_and_is_served() {
        let pool = NDArrayPool::new(1_000_000);
        let input = make_test_array(7);
        let proc = ServingProcessor {
            served: Arc::new(parking_lot::Mutex::new(None)),
        };
        let result = run_processor(&proc, input.array(), &pool, &[], None);
        assert_eq!(result.output_arrays.len(), 1);
        assert!(Arc::ptr_eq(&result.output_arrays[0], input.array()));
        assert!(Arc::ptr_eq(
            proc.served.lock().as_ref().unwrap(),
            input.array()
        ));
    }

    /// Sink processor: consumes arrays, returns nothing.
    struct SinkProcessor {
        count: std::sync::atomic::AtomicUsize,
    }

    impl NDPluginProcess for SinkProcessor {
        const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
        const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
        fn process_array(&self, _array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
            self.count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            ProcessResult::empty()
        }
        fn plugin_type(&self) -> &str {
            "Sink"
        }
    }

    /// An array of a driver no test reads.
    fn make_test_array(id: i32) -> PublishedArray {
        published(raw_test_array(id))
    }

    /// What a processor hands back: an array of no pool.
    fn raw_test_array(id: i32) -> Arc<NDArray> {
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = id;
        Arc::new(arr)
    }

    /// `array` as a driver no test reads publishes it.
    fn published(array: Arc<NDArray>) -> PublishedArray {
        DriverPool::standalone(0)
            .alloc_copy(&array)
            .unwrap()
            .share()
    }

    fn test_wiring() -> Arc<WiringRegistry> {
        Arc::new(WiringRegistry::new())
    }

    /// Fence: wait until the data thread has applied every param change
    /// submitted so far. `write_*_blocking` only guarantees the change is
    /// queued for the data plane; asserting on data-plane behaviour without
    /// this fence is a race.
    fn params_applied(handle: &PluginRuntimeHandle) {
        assert!(
            handle.wait_params_applied(std::time::Duration::from_secs(10)),
            "data thread did not apply queued param changes"
        );
    }

    /// Fence for tests that deliberately keep the input queue occupied.
    ///
    /// `params_applied` cannot serve there: its barrier also waits for the
    /// array queue to drain, which is the very state these tests hold.
    ///
    /// Timing out here is itself the finding these tests exist for: the data
    /// thread is the one that applies params, so a param that never lands
    /// means the loop is stuck inside work a callback thread should be doing.
    fn param_fifo_drained(handle: &PluginRuntimeHandle) {
        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(1);
        handle.param_tx.send(PluginParamMsg::Fence(ack_tx)).unwrap();
        assert!(
            ack_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .is_ok(),
            "the data thread stopped applying param writes — it is busy \
             inside work a callback thread should be doing"
        );
    }

    /// Poll `cond` until it holds; panic after 10 s. Waits on the observable
    /// state itself instead of sleeping a guessed duration, so a loaded
    /// machine cannot flake the test.
    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !cond() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Enable callbacks on a plugin handle (plugins default to disabled) and
    /// fence until the data thread has actually flipped the enable flag.
    fn enable_callbacks(handle: &PluginRuntimeHandle) {
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
            .unwrap();
        params_applied(handle);
    }

    /// Send an array via the sender from a sync test context.
    /// Uses a dedicated thread with a current-thread runtime to avoid
    /// interfering with the plugin's own runtime.
    fn send_array(sender: &NDArraySender, array: PublishedArray) {
        let sender = sender.clone();
        let jh = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(sender.publish(array));
        });
        jh.join().unwrap();
    }

    #[test]
    fn test_passthrough_runtime() {
        // Create downstream receiver
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("PASS1").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);

        // Send an array
        send_array(handle.array_sender(), make_test_array(42));

        // Should come out the other side
        let received = downstream_rx.blocking_recv().unwrap();
        assert_eq!(received.unique_id, 42);
    }

    /// A compression-aware plugin that hands on its input, or a fresh copy
    /// of it with its codec's level set.
    struct CodecPlugin {
        produce: bool,
    }

    impl NDPluginProcess for CodecPlugin {
        const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
        const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
        fn process_array(&self, array: &Arc<NDArray>, pool: &NDArrayPool) -> ProcessResult {
            if !self.produce {
                return ProcessResult::forward(array, vec![]);
            }
            let mut out = pool.alloc_copy(array).unwrap();
            out.codec = Some(level_five_lz4());
            ProcessResult::arrays(vec![Arc::new(out)])
        }
        fn plugin_type(&self) -> &str {
            "CodecPlugin"
        }
        fn compression_aware(&self) -> bool {
            true
        }
    }

    fn level_five_lz4() -> crate::codec::Codec {
        crate::codec::Codec {
            name: crate::codec::CodecName::LZ4,
            compressed_size: 4,
            level: 5,
            shuffle: 1,
            compressor: 2,
            original_data_type: NDDataType::UInt8,
        }
    }

    /// C copies an input a plugin hands on, and the copy keeps only the
    /// codec's name (NDPluginDriver.cpp:285-286, NDArrayPool.cpp:293); an
    /// output the plugin made goes out uncopied (NDPluginCodec.cpp:987).
    #[test]
    fn a_handed_on_input_keeps_only_its_codec_name() {
        for (name, produce) in [("CODECIN", false), ("CODECOUT", true)] {
            let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
            downstream_sender.admission().set_compression_aware(true);
            let mut output = NDArrayOutput::new();
            output.add(downstream_sender);
            let (handle, _jh) = create_plugin_runtime_with_output(
                &asyn_rs::asyn_record::claim_port(name).unwrap(),
                CodecPlugin { produce },
                1_000_000,
                10,
                0,
                output,
                "",
                test_wiring(),
                DEFAULT_PLUGIN_THREADS,
            );
            enable_callbacks(&handle);
            let mut frame = DriverPool::standalone(0)
                .alloc_copy(&raw_test_array(7))
                .unwrap();
            frame.fill().set_codec(Some(level_five_lz4()));
            send_array(handle.array_sender(), frame.share());

            let received = downstream_rx.blocking_recv().unwrap();
            let codec = received.codec.as_ref().unwrap();
            assert!(matches!(codec.name, crate::codec::CodecName::LZ4), "{name}");
            let expected = if produce {
                [5, 1, 2]
            } else {
                [crate::codec::Codec::UNSET; 3]
            };
            assert_eq!(
                [codec.level, codec.shuffle, codec.compressor],
                expected,
                "{name}"
            );
        }
    }

    /// C `endProcessCallbacks` copies a handed-on input into the pool it came
    /// from with NDArrayCallbacks 1 (NDPluginDriver.cpp:285-286, :419) and
    /// with 0 only reserves it (:274-279), so only the first charges the
    /// detector's pool a second buffer.
    #[test]
    fn a_handed_on_input_is_copied_into_its_pool_only_with_array_callbacks() {
        for (name, callbacks) in [("HANDON1", 1), ("HANDON0", 0)] {
            let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
            let mut output = NDArrayOutput::new();
            output.add(downstream_sender);
            let (handle, _jh) = create_plugin_runtime_with_output(
                &asyn_rs::asyn_record::claim_port(name).unwrap(),
                StampProcessor { stamp: 0, addr: 0 },
                1_000_000,
                10,
                0,
                output,
                "",
                test_wiring(),
                DEFAULT_PLUGIN_THREADS,
            );
            enable_callbacks(&handle);
            handle
                .port_runtime()
                .port_handle()
                .write_int32_blocking(handle.ndarray_params.array_callbacks, 0, callbacks)
                .unwrap();
            params_applied(&handle);
            let detector = DriverPool::standalone(1_000_000);
            let frame = detector
                .alloc(vec![NDDimension::new(4)], NDDataType::UInt8)
                .unwrap()
                .share();
            send_array(handle.array_sender(), frame.clone());
            let received = (callbacks == 1).then(|| downstream_rx.blocking_recv().unwrap());
            let queued = detector.owner().queued().clone();
            wait_until("the detector's queue to drain", || queued.get() == 0);
            if let Some(received) = &received {
                assert!(!Arc::ptr_eq(received, frame.array()));
                assert_eq!(received.pool_id(), detector.id());
            }
            assert_eq!(detector.num_alloc_buffers(), 1 + callbacks as u32, "{name}");
        }
    }

    /// NDPluginStdArrays at NDArrayCallbacks 0 serves its waveform from the
    /// frame it was handed: C copies only past the NDArrayCallbacks return
    /// (NDPluginDriver.cpp:274-286).
    #[test]
    fn a_std_arrays_frame_at_array_callbacks_0_is_served_uncopied() {
        let served = Arc::new(parking_lot::Mutex::new(None));
        let (handle, _jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("SERVE0").unwrap(),
            ServingProcessor {
                served: Arc::clone(&served),
            },
            1_000_000,
            10,
            0,
            NDArrayOutput::new(),
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.ndarray_params.array_callbacks, 0, 0)
            .unwrap();
        params_applied(&handle);
        let detector = DriverPool::standalone(1_000_000);
        let frame = detector
            .alloc(vec![NDDimension::new(4)], NDDataType::UInt8)
            .unwrap()
            .share();
        send_array(handle.array_sender(), frame.clone());
        wait_until("the waveform to be served", || served.lock().is_some());
        assert!(Arc::ptr_eq(served.lock().as_ref().unwrap(), frame.array()));
        assert_eq!(detector.num_alloc_buffers(), 1);
    }

    type StdArrayPosts = (
        Vec<(Option<asyn_rs::interfaces::InterfaceType>, String)>,
        Vec<String>,
    );

    /// When the port's interrupt edge sweep runs.
    #[derive(Clone, Copy)]
    enum Sweep {
        /// The gate is open from the start.
        First,
        /// The gate opens before the frame, its sweep runs after the frame's
        /// posts.
        AfterTheFrame,
    }

    /// What one frame of `data` through a StdArrays plugin posts on
    /// STD_ARRAY_DATA: each binding on `bound` records its post's interface
    /// and value, and an observer, which binds nothing, every post's
    /// interface in order, NDDimensions' as `dims`.
    fn std_array_posts(
        port: &str,
        data: crate::ndarray::NDDataBuffer,
        bound: &[asyn_rs::interfaces::InterfaceType],
        sweep: Sweep,
    ) -> StdArrayPosts {
        use epics_libcom_rs::runtime::interrupt_accept::{EdgeDone, InterruptGate};
        fn shown(v: &ParamValue) -> String {
            match v {
                ParamValue::Int8Array(a) => format!("Int8 {a:?}"),
                ParamValue::Int16Array(a) => format!("Int16 {a:?}"),
                ParamValue::Int32Array(a) => format!("Int32 {a:?}"),
                ParamValue::Int64Array(a) => format!("Int64 {a:?}"),
                ParamValue::Float32Array(a) => format!("Float32 {a:?}"),
                ParamValue::Float64Array(a) => format!("Float64 {a:?}"),
                other => other.type_name().to_string(),
            }
        }
        let claim = asyn_rs::asyn_record::claim_port(port).unwrap();
        let (handle, _jh) = create_plugin_runtime_with_output(
            &claim,
            ServingProcessor {
                served: Arc::default(),
            },
            1_000_000,
            10,
            0,
            NDArrayOutput::new(),
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle().clone();
        let release = Arc::new(std::sync::Barrier::new(2));
        let (gate, opener) = match sweep {
            Sweep::First => {
                let gate = InterruptGate::opened();
                claim.publish(port.clone(), &gate).unwrap();
                (gate, None)
            }
            Sweep::AfterTheFrame => {
                let gate = InterruptGate::new();
                // Runs ahead of the port's own edge callback, the gate open.
                let held = release.clone();
                gate.on_open(move || {
                    held.wait();
                    EdgeDone::Inline
                });
                claim.publish(port.clone(), &gate).unwrap();
                let opening = gate.clone();
                let opener = std::thread::spawn(move || opening.set(true));
                wait_until("the gate to open", || gate.is_open());
                (gate, Some(opener))
            }
        };
        enable_callbacks(&handle);
        let std = port
            .drv_user_create_blocking(&asyn_rs::port::DrvUserRequest::new("STD_ARRAY_DATA", 0))
            .unwrap()
            .reason;
        let dims = handle.ndarray_params.array_dimensions;
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let _bindings: Vec<_> = bound
            .iter()
            .map(|&iface| {
                let log = seen.clone();
                port.interrupts()
                    .register_sync_callback(
                        asyn_rs::interrupt::InterruptFilter {
                            reason: Some(std),
                            addr: Some(0),
                            iface: Some(iface),
                            ..Default::default()
                        },
                        move |iv| log.lock().push((iv.iface, shown(&iv.value))),
                    )
                    .unwrap()
            })
            .collect();
        let mut observer = port.interrupts().subscribe_async(256, 4096);
        let array = NDArray::with_data(vec![NDDimension::new(data.len())], data);
        send_array(handle.array_sender(), published(Arc::new(array)));
        let counter = handle.ndarray_params.array_counter;
        wait_until("the frame to be counted", || {
            port.read_int32_blocking(counter, 0).is_ok_and(|v| v == 1)
        });
        params_applied(&handle);
        // The flush is enqueued, not run: a round trip runs behind it.
        port.read_int32_blocking(counter, 0).unwrap();
        if let Some(opener) = opener {
            release.wait();
            opener.join().unwrap();
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(gate.jobs_settled());
            wait_until("the edge sweep", || {
                port.interrupts().edge_sweeps().is_clear()
            });
        }
        let mut observed = Vec::new();
        loop {
            match observer.try_recv() {
                Ok(v) if v.reason == std => observed.push(format!("{:?}", v.iface)),
                Ok(v) if v.reason == dims => observed.push("dims".to_string()),
                Ok(_) | Err(asyn_rs::interrupt::AsyncTryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
        let posts = seen.lock().clone();
        (posts, observed)
    }

    /// NDPluginStdArrays converts the frame from its own type into each
    /// interface a client is bound to, and posts it on that interface after
    /// beginProcessCallbacks' dims post (NDPluginStdArrays.cpp:38-52,
    /// :167-197): an unsigned frame's top value reaches an Int32 and a
    /// Float64 waveform as `convert` gives it.
    #[test]
    fn a_std_arrays_frame_converts_from_its_own_type_per_bound_interface() {
        use crate::ndarray::NDDataBuffer;
        use asyn_rs::interfaces::InterfaceType::{Float64Array, Int32Array};
        for (port, data, int32, float64) in [
            (
                "STDU8",
                NDDataBuffer::from_vec::<u8>(vec![255]),
                "[255]",
                "[255.0]",
            ),
            (
                "STDU16",
                NDDataBuffer::from_vec::<u16>(vec![65535]),
                "[65535]",
                "[65535.0]",
            ),
            (
                "STDU32",
                NDDataBuffer::from_vec::<u32>(vec![u32::MAX]),
                "[-1]",
                "[4294967295.0]",
            ),
        ] {
            let (posts, observed) =
                std_array_posts(port, data, &[Int32Array, Float64Array], Sweep::First);
            assert_eq!(
                posts,
                [
                    (Some(Int32Array), format!("Int32 {int32}")),
                    (Some(Float64Array), format!("Float64 {float64}")),
                ],
                "{port}"
            );
            assert_eq!(
                observed,
                ["dims", "Some(Int32Array)", "Some(Float64Array)"],
                "{port}"
            );
        }
    }

    /// An interface with no client takes no conversion and no post: C's
    /// `arrayInterruptCallback` converts only for a node on its list.
    #[test]
    fn a_std_arrays_frame_with_no_bound_interface_posts_nothing() {
        let (posts, observed) = std_array_posts(
            "STDNONE",
            crate::ndarray::NDDataBuffer::from_vec::<u8>(vec![255]),
            &[],
            Sweep::First,
        );
        assert!(posts.is_empty());
        assert_eq!(observed, ["dims"]);
    }

    /// A frame the open gate delivered before the port's edge sweep ran: the
    /// sweep posts no dims of its own, whatever the frame left in
    /// `dimsPrev_`.
    #[test]
    fn a_std_arrays_frame_before_the_edge_sweep_is_posted_once() {
        use asyn_rs::interfaces::InterfaceType::Int32Array;
        let (posts, observed) = std_array_posts(
            "STDSWEEP",
            crate::ndarray::NDDataBuffer::from_vec::<u8>(vec![255]),
            &[Int32Array],
            Sweep::AfterTheFrame,
        );
        assert_eq!(posts, [(Some(Int32Array), "Int32 [255]".to_string())]);
        assert_eq!(observed, ["dims", "Some(Int32Array)"]);
    }

    /// Sets its own STAMP at `addr` to ten times the frame's id and forwards
    /// the frame.
    struct StampProcessor {
        stamp: usize,
        addr: i32,
    }

    impl NDPluginProcess for StampProcessor {
        const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
        const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
        fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
            ProcessResult::forward(
                array,
                vec![ParamUpdate::int32_addr(
                    self.stamp,
                    self.addr,
                    array.unique_id * 10,
                )],
            )
        }
        fn plugin_type(&self) -> &str {
            "Stamp"
        }
        fn max_addr(&self) -> usize {
            self.addr as usize + 1
        }
        fn register_params(&mut self, base: &mut PortDriverBase) -> Result<(), AsynError> {
            self.stamp = base.create_param("STAMP", asyn_rs::param::ParamType::Int32)?;
            Ok(())
        }
    }

    /// C `endProcessCallbacks(pArray, true, true)` runs `getAttributes` on
    /// its copy (NDPluginDriver.cpp:285-291): the plugin's own set reaches
    /// the output with the values the plugin set for this frame, and the
    /// frame it was handed keeps its attributes.
    #[test]
    fn a_plugin_attributes_file_reaches_its_output() {
        use crate::attributes::NDAttrValue;
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);
        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("ATTRS1").unwrap(),
            StampProcessor { stamp: 0, addr: 0 },
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);
        let port = handle.port_runtime().port_handle();
        let xml = r#"<Attributes>
            <Attribute name="Stamp" type="PARAM" source="STAMP" datatype="INT"/>
        </Attributes>"#;
        port.submit_blocking(
            asyn_rs::request::RequestOp::OctetWrite {
                data: xml.as_bytes().to_vec(),
            },
            AsynUser::new(handle.ndarray_params.attributes_file),
        )
        .unwrap();

        let input = make_test_array(3);
        send_array(handle.array_sender(), input.clone());
        let received = downstream_rx.blocking_recv().unwrap();
        assert_eq!(
            received.attributes.get("Stamp").map(|a| a.value.clone()),
            Some(NDAttrValue::Int32(30))
        );
        assert!(input.attributes.get("Stamp").is_none());
    }

    /// C stores a frame's params, delivers the frame and then calls
    /// `callParamCallbacks`, all under the port lock (NDPluginDriver.cpp:332,
    /// :539): a consumer still holding the frame reads its STAMP from the
    /// port, and STAMP has not been posted — on list 0, which C's
    /// `callParamCallbacks()` posts, and on a further list alike.
    fn a_held_frame_reads_its_stamp_unposted(port_name: &str, blocking_callbacks: i32, addr: i32) {
        use epics_libcom_rs::runtime::task::block_on_sync;
        use std::sync::atomic::AtomicBool;
        let (mut downstream, mut downstream_rx) = ndarray_channel(&format!("{port_name}_DOWN"), 10);
        // A blocking consumer holds its producer until it drops the frame.
        downstream.set_mode_flags(
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
        );
        let mut output = NDArrayOutput::new();
        output.add(downstream);
        let claim = asyn_rs::asyn_record::claim_port(port_name).unwrap();
        let (handle, _jh) = create_plugin_runtime_with_output(
            &claim,
            StampProcessor { stamp: 0, addr },
            1_000_000,
            10,
            blocking_callbacks,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle().clone();
        claim
            .publish(
                port.clone(),
                &epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened(),
            )
            .unwrap();
        enable_callbacks(&handle);
        let stamp = port
            .drv_user_create_blocking(&asyn_rs::port::DrvUserRequest::new("STAMP", 0))
            .unwrap()
            .reason;
        let posts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let log = posts.clone();
        let _binding = port
            .interrupts()
            .register_sync_callback(
                asyn_rs::interrupt::InterruptFilter {
                    reason: Some(stamp),
                    addr: Some(addr),
                    ..Default::default()
                },
                move |iv| {
                    if let ParamValue::Int32(v) = iv.value {
                        log.lock().push(v);
                    }
                },
            )
            .unwrap();
        let sender = handle.array_sender().clone();
        let producer =
            std::thread::spawn(move || block_on_sync(sender.publish(make_test_array(3))).unwrap());
        let held = block_on_sync(downstream_rx.recv_msg()).unwrap().unwrap();
        assert_eq!(port.read_int32_blocking(stamp, addr).ok(), Some(30));
        // The read's round trip ran behind anything enqueued before it.
        assert!(posts.lock().is_empty(), "posted before the delivery ended");
        drop(held);
        producer.join().unwrap();
        wait_until("the STAMP post", || !posts.lock().is_empty());
        assert_eq!(*posts.lock(), [30]);
    }

    #[test]
    fn a_callback_thread_frame_is_stored_before_and_posted_after_its_delivery() {
        a_held_frame_reads_its_stamp_unposted("ORDERQ", 0, 0);
    }

    #[test]
    fn a_blocking_frame_is_stored_before_and_posted_after_its_delivery() {
        a_held_frame_reads_its_stamp_unposted("ORDERB", 1, 0);
    }

    #[test]
    fn a_further_list_is_stored_before_and_posted_after_its_delivery() {
        a_held_frame_reads_its_stamp_unposted("ORDERL", 0, 1);
    }

    /// Sets a float into its int STAMP, a set C's `setDoubleParam` refuses.
    struct MistypedStamp {
        stamp: usize,
    }

    impl NDPluginProcess for MistypedStamp {
        const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
        const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
        fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
            ProcessResult::forward(array, vec![ParamUpdate::float64(self.stamp, 1.5)])
        }
        fn plugin_type(&self) -> &str {
            "MistypedStamp"
        }
        fn register_params(&mut self, base: &mut PortDriverBase) -> Result<(), AsynError> {
            self.stamp = base.create_param("STAMP", asyn_rs::param::ParamType::Int32)?;
            Ok(())
        }
    }

    /// C sets each plugin param once and `getAttributes` only reads the
    /// store, so a refused set prints its line once, and the attribute
    /// reads the stored value.
    #[test]
    fn evaluating_the_attributes_sets_nothing() {
        use crate::attributes::NDAttrValue;
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);
        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("ATTRS2").unwrap(),
            MistypedStamp { stamp: 0 },
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);
        let port = handle.port_runtime().port_handle();
        let log = std::env::temp_dir().join(format!("attrs2-trace-{}.log", std::process::id()));
        let file = std::fs::File::create(&log).unwrap();
        port.trace().set_trace_file(
            Some("ATTRS2"),
            asyn_rs::trace::TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );
        let xml = r#"<Attributes>
            <Attribute name="Stamp" type="PARAM" source="STAMP" datatype="INT"/>
        </Attributes>"#;
        port.submit_blocking(
            asyn_rs::request::RequestOp::OctetWrite {
                data: xml.as_bytes().to_vec(),
            },
            AsynUser::new(handle.ndarray_params.attributes_file),
        )
        .unwrap();
        let stamp = port
            .drv_user_create_blocking(&asyn_rs::port::DrvUserRequest::new("STAMP", 0))
            .unwrap()
            .reason;
        port.write_int32_blocking(stamp, 0, 7).unwrap();

        send_array(handle.array_sender(), make_test_array(3));
        let received = downstream_rx.blocking_recv().unwrap();
        params_applied(&handle);
        assert_eq!(
            received.attributes.get("Stamp").map(|a| a.value.clone()),
            Some(NDAttrValue::Int32(7))
        );
        // The frame's batch, ArrayCounter with the refused STAMP, lands after
        // its delivery.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while port
            .read_int32_blocking(handle.ndarray_params.array_counter, 0)
            .unwrap()
            < 1
        {
            assert!(std::time::Instant::now() < deadline, "batch not applied");
            std::thread::yield_now();
        }
        let text = std::fs::read_to_string(&log).unwrap();
        std::fs::remove_file(&log).ok();
        let refused = text
            .lines()
            .filter(|l| l.contains("setDoubleParam"))
            .count();
        assert_eq!(refused, 1, "{text}");
    }

    #[test]
    fn the_attributes_read_this_frames_begin_sets() {
        use crate::attributes::NDAttrValue;
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);
        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("ATTRBEGIN").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);
        let xml = r#"<Attributes>
            <Attribute name="Counter" type="PARAM" source="ARRAY_COUNTER" datatype="INT"/>
            <Attribute name="Id" type="PARAM" source="UNIQUE_ID" datatype="INT"/>
        </Attributes>"#;
        handle
            .port_runtime()
            .port_handle()
            .submit_blocking(
                asyn_rs::request::RequestOp::OctetWrite {
                    data: xml.as_bytes().to_vec(),
                },
                AsynUser::new(handle.ndarray_params.attributes_file),
            )
            .unwrap();
        params_applied(&handle);
        for (counter, id) in [(1, 5), (2, 6)] {
            send_array(handle.array_sender(), make_test_array(id));
            let received = downstream_rx.blocking_recv().unwrap();
            let read = |name: &str| received.attributes.get(name).map(|a| a.value.clone());
            assert_eq!(read("Counter"), Some(NDAttrValue::Int32(counter)));
            assert_eq!(read("Id"), Some(NDAttrValue::Int32(id)));
        }
    }

    #[test]
    fn test_sink_runtime() {
        let (handle, _data_jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("SINK1").unwrap(),
            SinkProcessor {
                count: std::sync::atomic::AtomicUsize::new(0),
            },
            1_000_000,
            10,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);

        // Send arrays - they should be consumed silently
        send_array(handle.array_sender(), make_test_array(1));
        send_array(handle.array_sender(), make_test_array(2));

        // ArrayCounter advances once per processed frame — both consumed.
        let port = handle.port_runtime().port_handle().clone();
        let counter = handle.ndarray_params.array_counter;
        wait_until("sink to process both arrays", || {
            port.read_int32_blocking(counter, 0).is_ok_and(|v| v == 2)
        });
        assert_eq!(handle.port_name(), "SINK1");
    }

    #[test]
    fn test_plugin_type_param() {
        let (handle, _data_jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("TYPE_TEST").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );

        // Verify port name
        assert_eq!(handle.port_name(), "TYPE_TEST");
        assert_eq!(handle.port_runtime().port_name(), "TYPE_TEST");
    }

    /// C `beginProcessCallbacks` `setTimeStamp(&pArray->epicsTS)`
    /// (NDPluginDriver.cpp:232): the port timestamp a cycle's callbacks and
    /// reads carry is the input's epicsTS, not the output's nor the epoch.
    #[test]
    fn a_plugin_cycle_stamps_its_callbacks_with_the_input_epics_ts() {
        struct Restamp;
        impl NDPluginProcess for Restamp {
            const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
            const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
            const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
            fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
                let mut out = NDArray::clone(array);
                out.timestamp = crate::timestamp::EpicsTimestamp { sec: 9, nsec: 9 };
                ProcessResult::arrays(vec![Arc::new(out)])
            }
            fn plugin_type(&self) -> &str {
                "Restamp"
            }
        }
        let (ds, _rx) = ndarray_channel("DS_STAMP", 10);
        let mut output = NDArrayOutput::new();
        output.add(ds);
        let (handle, _jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("STAMP").unwrap(),
            Restamp,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);
        let port = handle.port_runtime().port_handle().clone();
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.timestamp = crate::timestamp::EpicsTimestamp {
            sec: 1234,
            nsec: 5678,
        };
        let input = arr.timestamp.to_system_time();
        send_array(handle.array_sender(), published(Arc::new(arr)));
        let counter = handle.ndarray_params.array_counter;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while port.read_int32_blocking(counter, 0).unwrap() != 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "the frame was not processed"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let stamp = port
            .with_driver_blocking::<PluginPortDriver, _>(|d| d.base().get_time_stamp())
            .unwrap();
        assert_eq!(stamp, input);
    }

    #[test]
    fn test_ndtimestamp_param_is_the_standalone_double() {
        // R8-66 family: C `setDoubleParam(NDTimeStamp, pArray->timeStamp)`
        // (NDPluginDriver.cpp:217) publishes the array's standalone double —
        // which a driver with a hardware clock sets independently of epicsTS —
        // while NDEpicsTSSec/nSec carry epicsTS (`:218-219`). The plugin runtime
        // published `timestamp.as_f64()` for all three, so NDTimeStamp_RBV
        // reported the epicsTS-derived value.
        let (ds, _rx) = ndarray_channel("DS_TS", 10);
        let mut output = NDArrayOutput::new();
        output.add(ds);
        let (handle, _jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("TS_PARAM").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);
        let port = handle.port_runtime().port_handle().clone();

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.timestamp = crate::timestamp::EpicsTimestamp {
            sec: 1234,
            nsec: 5678,
        };
        arr.time_stamp = 100.5; // hardware clock, unrelated to epicsTS
        send_array(handle.array_sender(), published(Arc::new(arr)));
        std::thread::sleep(std::time::Duration::from_millis(50));

        assert_eq!(
            port.read_float64_blocking(handle.ndarray_params.timestamp_rbv, 0)
                .unwrap(),
            100.5,
            "NDTimeStamp publishes pArray->timeStamp"
        );
        assert_eq!(
            port.read_int32_blocking(handle.ndarray_params.epics_ts_sec, 0)
                .unwrap(),
            1234
        );
        assert_eq!(
            port.read_int32_blocking(handle.ndarray_params.epics_ts_nsec, 0)
                .unwrap(),
            5678
        );
    }

    #[test]
    fn test_shutdown_on_handle_drop() {
        let (handle, data_jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("SHUTDOWN_TEST").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );

        // Drop the handle (closes sender channel, which should cause data thread to exit)
        let sender = handle.array_sender().clone();
        drop(handle);
        drop(sender);

        // Data thread should terminate
        let result = data_jh.join();
        assert!(result.is_ok());
    }

    /// C `NDPluginDriver::writeOctet` hands a base-class parameter to
    /// `asynNDArrayDriver::writeOctet` (NDPluginDriver.cpp:839-841), so a
    /// plugin's `FilePath` write creates the directory under `CreateDir`.
    #[test]
    fn a_plugin_file_path_write_reaches_the_base_class() {
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("PLUGIN_PATH").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        let p = port
            .with_driver_blocking(|d: &mut PluginPortDriver| d.ndarray_params)
            .unwrap();
        let root = std::env::temp_dir().join(format!("plugin-path-{}", std::process::id()));
        let target = root.join("sub");
        port.write_int32_blocking(p.create_dir, 0, 1).unwrap();
        port.submit_blocking(
            asyn_rs::request::RequestOp::OctetWrite {
                data: format!("{}/", target.display()).into_bytes(),
            },
            AsynUser::new(p.file_path),
        )
        .unwrap();
        let created = target.is_dir();
        let _ = std::fs::remove_dir_all(&root);
        assert!(created);
        assert_eq!(port.read_int32_blocking(p.file_path_exists, 0).unwrap(), 1);
    }

    /// A base-class octet write goes through `asynNDArrayDriver::writeOctet`
    /// (NDPluginDriver.cpp:839-841), which closes with its own line
    /// (asynNDArrayDriver.cpp:544-546); a plugin param's does not.
    #[test]
    fn a_plugin_base_class_octet_write_prints_the_asyn_nd_array_driver_line() {
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("PLUGIN_OCTET_LINE").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        let log = std::env::temp_dir().join(format!("plugin-octet-{}.log", std::process::id()));
        let file = std::fs::File::create(&log).unwrap();
        port.trace().set_trace_file(
            Some("PLUGIN_OCTET_LINE"),
            asyn_rs::trace::TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );
        port.trace().set_trace_mask(
            Some("PLUGIN_OCTET_LINE"),
            TraceMask::ERROR | TraceMask::IO_DRIVER,
        );
        let file_name = handle.ndarray_params.file_name;
        let plugin_type = handle.plugin_params.plugin_type;
        for (reason, value) in [(file_name, "img"), (plugin_type, "x")] {
            port.submit_blocking(
                asyn_rs::request::RequestOp::OctetWrite {
                    data: value.as_bytes().to_vec(),
                },
                AsynUser::new(reason),
            )
            .unwrap();
        }
        let text = std::fs::read_to_string(&log).unwrap();
        let _ = std::fs::remove_file(&log);
        let lines: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("asynNDArrayDriver:writeOctet"))
            .collect();
        assert_eq!(lines.len(), 1, "{text}");
        assert!(
            lines[0].ends_with(&format!(
                "asynNDArrayDriver:writeOctet: function={file_name}, paramName=FILE_NAME, value=img"
            )),
            "{text}"
        );
    }

    /// The writes C `NDPluginDriver::writeInt32`/`writeOctet` fail
    /// (NDPluginDriver.cpp:605-611, 973-994) fail here too, so the record
    /// takes its WRITE alarm; a clamp is what the readback shows.
    /// C prints why NDPluginDriver refused (NDPluginDriver.cpp:974-976,
    /// :606-609) and then
    /// `"%s::%s ERROR, status=%d, function=%d, paramName=%s, value=%d, connectedToArrayPort_=%d\n"`
    /// (:766-769), and leaves errorMessage empty.
    #[test]
    fn base_refusals_print_c_lines_and_leave_error_message_empty() {
        let wiring = test_wiring();
        wiring.register_output(
            "UP",
            Arc::new(parking_lot::Mutex::new(NDArrayOutput::new())),
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut drv, _, _) = PluginPortDriver::new(
            "BASELINES",
            "Passthrough",
            10,
            0,
            &"UP".into(),
            tx,
            PassthroughProcessor,
            None,
            1_000_000,
            wiring,
            PLUGIN_MAX_THREADS,
        )
        .unwrap();
        let (threads, addr) = (
            drv.plugin_params.num_threads,
            drv.plugin_params.nd_array_addr,
        );
        let refused = |keep| WriteVerdict::Refuse {
            keep,
            message: String::new(),
        };

        let out = drv.base_int32(threads, 98).unwrap();
        assert_eq!(out.verdict, refused(1));
        assert_eq!(
            out.trace,
            vec![
                (
                    TraceMask::ERROR,
                    "NDPluginDriver::createCallbackThreads error, numThreads=98 must be <= maxThreads=1, setting to 1\n".to_string()
                ),
                (
                    TraceMask::ERROR,
                    format!("NDPluginDriver::writeInt32 ERROR, status=3, function={threads}, paramName=NUM_THREADS, value=98, connectedToArrayPort_=1\n")
                ),
            ]
        );

        drv.base
            .set_string_param(drv.plugin_params.nd_array_port, 0, "BOGUS")
            .unwrap();
        let out = drv.base_int32(addr, 2).unwrap();
        assert_eq!(out.verdict, refused(2));
        assert_eq!(
            out.trace,
            vec![
                (
                    TraceMask::ERROR,
                    "NDPluginDriver::connectToArrayPort Error calling pasynManager->connectDevice to array port BOGUS address 2, status=3, error=asynManager:connectDevice port BOGUS not found\n".to_string()
                ),
                (
                    TraceMask::ERROR,
                    format!("NDPluginDriver::writeInt32 ERROR, status=3, function={addr}, paramName=NDARRAY_ADDR, value=2, connectedToArrayPort_=0\n")
                ),
            ]
        );
    }

    /// A side param at another address is stored but posted only by that
    /// address's next `callParamCallbacks`, as NDPluginAttribute's Reset
    /// (:122-136) leaves channels 1.. until their next frame.
    #[test]
    fn a_write_posts_only_its_own_address() {
        let wiring = test_wiring();
        wiring.register_output(
            "UP",
            Arc::new(parking_lot::Mutex::new(NDArrayOutput::new())),
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let processor = ValidatingProcessor {
            addrs: 2,
            ..Default::default()
        };
        let ids = processor.ids.clone();
        let (mut drv, _, _) = PluginPortDriver::new(
            "POSTADDR",
            "Validating",
            10,
            0,
            &"UP".into(),
            tx,
            processor,
            None,
            1_000_000,
            wiring,
            PLUGIN_MAX_THREADS,
        )
        .unwrap();
        let (limit, twice, _, _) = *ids.lock();
        drv.base
            .own_interrupts(epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened());
        for a in 0..2 {
            drv.base.params.take_changed(a).unwrap();
        }
        drv.io_write_int32(&mut AsynUser::new(limit).with_addr(0), 7)
            .unwrap();
        assert_eq!(drv.base.get_int32_param(twice, 1).unwrap(), 14);
        assert_eq!(drv.base.params.take_changed(1).unwrap(), vec![twice]);
        assert!(drv.base.params.take_changed(0).unwrap().is_empty());
    }

    /// NDPluginFile.cpp:189-193: the array a ReadFile write reads goes
    /// downstream from the write.
    #[test]
    fn an_array_a_write_publishes_reaches_downstream() {
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);
        let processor = ValidatingProcessor::default();
        let ids = processor.ids.clone();
        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("PUBLISH").unwrap(),
            processor,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let (limit, ..) = *ids.lock();
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(limit, 0, 5)
            .unwrap();
        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 5);
    }

    /// A written array meets the MaxByteRate throttle as a processed one
    /// does: NDPluginFile hands it to endProcessCallbacks
    /// (NDPluginFile.cpp:193, :252).
    #[test]
    fn an_array_a_write_publishes_is_throttled() {
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);
        let processor = ValidatingProcessor::default();
        let ids = processor.ids.clone();
        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("PUBLISH_THROTTLE").unwrap(),
            processor,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        // A 1 byte/s bucket cannot pass the 4-byte array.
        port.write_float64_blocking(handle.plugin_params.max_byte_rate, 0, 1.0)
            .unwrap();
        let (limit, ..) = *ids.lock();
        port.write_int32_blocking(limit, 0, 5).unwrap();
        params_applied(&handle);
        wait_until("the drop to post", || {
            port.read_int32_blocking(handle.plugin_params.dropped_output_arrays, 0)
                .is_ok_and(|v| v == 1)
        });
        // The drop posts after the publish, so a delivery would be queued.
        assert!(downstream_rx.try_recv_msg().is_none());
    }

    /// A written array out of uniqueId order waits in the sort buffer
    /// (NDPluginDriver.cpp:305-325) until sorting stops.
    #[test]
    fn an_array_a_write_publishes_is_sorted() {
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);
        let processor = ValidatingProcessor::default();
        let ids = processor.ids.clone();
        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("PUBLISH_SORT").unwrap(),
            processor,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        let p = handle.plugin_params;
        port.write_float64_blocking(p.sort_time, 0, 30.0).unwrap();
        port.write_int32_blocking(p.sort_mode, 0, 1).unwrap();
        let (limit, ..) = *ids.lock();
        // uniqueId 5 does not follow 0.
        port.write_int32_blocking(limit, 0, 5).unwrap();
        params_applied(&handle);
        assert!(downstream_rx.try_recv_msg().is_none());
        port.write_int32_blocking(p.sort_mode, 0, 0).unwrap();
        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 5);
    }

    /// A written array is `pArrays[0]` with NDArrayCallbacks 0
    /// (NDPluginDriver.cpp:273-280), so PoolPreAllocBuffers has dimensions
    /// to allocate from.
    #[test]
    fn an_array_a_write_publishes_is_the_pre_alloc_template() {
        let processor = ValidatingProcessor::default();
        let ids = processor.ids.clone();
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("PUBLISH_CACHE").unwrap(),
            processor,
            1_000_000,
            10,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        let p = handle.ndarray_params;
        port.write_int32_blocking(p.array_callbacks, 0, 0).unwrap();
        let (limit, ..) = *ids.lock();
        port.write_int32_blocking(limit, 0, 5).unwrap();
        port.write_int32_blocking(p.pool_num_pre_alloc_buffers, 0, 2)
            .unwrap();
        port.write_int32_blocking(p.pool_pre_alloc, 0, 1).unwrap();
        port.write_int32_blocking(p.pool_poll_stats, 0, 1).unwrap();
        params_applied(&handle);
        assert_eq!(port.read_int32_blocking(p.pool_free_buffers, 0).unwrap(), 2);
    }

    /// Counts a param down from inside a GO write, as NDPluginFile's capture
    /// flush counts NumCaptured (NDPluginFile.cpp:315-321), noting how many
    /// callbacks had fired after each post.
    #[derive(Default)]
    struct Countdown {
        ids: Arc<parking_lot::Mutex<(usize, usize)>>,
        seen: Arc<parking_lot::Mutex<Vec<i32>>>,
        fired: Arc<parking_lot::Mutex<Vec<usize>>>,
    }

    impl NDPluginProcess for Countdown {
        const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
        const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
        fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
            ProcessResult::forward(array, vec![])
        }
        fn plugin_type(&self) -> &str {
            "Countdown"
        }
        fn register_params(&mut self, base: &mut PortDriverBase) -> Result<(), AsynError> {
            let go = base.create_param("GO", asyn_rs::param::ParamType::Int32)?;
            let count = base.create_param("COUNT", asyn_rs::param::ParamType::Int32)?;
            *self.ids.lock() = (go, count);
            Ok(())
        }
        fn validate_int32(
            &self,
            reason: usize,
            _addr: i32,
            _value: i32,
            verdict: WriteVerdict<i32>,
            params: &mut WritePort<'_>,
        ) -> WriteOutcome<i32> {
            let (go, count) = *self.ids.lock();
            if reason != go {
                return verdict.into();
            }
            for n in (0..3).rev() {
                params.post(vec![ParamUpdate::int32(count, n)]);
                self.fired.lock().push(self.seen.lock().len());
            }
            WriteOutcome::from(verdict).set_int32(count, 0, 0)
        }
    }

    #[test]
    fn a_write_posts_each_value_inside_the_write() {
        let wiring = test_wiring();
        wiring.register_output(
            "UP",
            Arc::new(parking_lot::Mutex::new(NDArrayOutput::new())),
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let processor = Countdown::default();
        let (ids, seen, fired) = (
            processor.ids.clone(),
            processor.seen.clone(),
            processor.fired.clone(),
        );
        let (mut drv, _, _) = PluginPortDriver::new(
            "COUNTDOWN",
            "Countdown",
            10,
            0,
            &"UP".into(),
            tx,
            processor,
            None,
            1_000_000,
            wiring,
            PLUGIN_MAX_THREADS,
        )
        .unwrap();
        drv.base
            .own_interrupts(epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened());
        let (go, count) = *ids.lock();
        drv.base.set_int32_param(count, 0, 3).unwrap();
        drv.base.call_param_callbacks(0).unwrap();
        let log = seen.clone();
        let _sync = drv
            .base
            .interrupts
            .register_sync_callback(
                asyn_rs::interrupt::InterruptFilter {
                    reason: Some(count),
                    ..Default::default()
                },
                move |iv| {
                    if let ParamValue::Int32(v) = iv.value {
                        log.lock().push(v)
                    }
                },
            )
            .unwrap();
        drv.io_write_int32(&mut AsynUser::new(go), 1).unwrap();
        assert_eq!(*fired.lock(), [1, 2, 3], "each post fires before the next");
        assert_eq!(
            *seen.lock(),
            [2, 1, 0],
            "the write's own 0 posts nothing new"
        );
    }

    /// A frame callback's post reaches the port through its queue.
    #[test]
    fn a_frame_callback_post_reaches_the_port() {
        #[derive(Default)]
        struct Poster {
            count: Arc<parking_lot::Mutex<usize>>,
            attributes: Option<PluginAttributes>,
            /// UniqueId, and its value just after the post.
            unique_id: Arc<parking_lot::Mutex<Option<usize>>>,
            seen: Arc<parking_lot::Mutex<Option<i32>>>,
            /// UniqueId as the post reads it.
            read: Arc<parking_lot::Mutex<Option<i32>>>,
        }
        impl NDPluginProcess for Poster {
            const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
            const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
            const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
            fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
                let count = *self.count.lock();
                let mut post = PortPost::of(self.attributes.as_ref());
                post.post(vec![ParamUpdate::int32(count, 7)]);
                *self.read.lock() = self.unique_id.lock().and_then(|r| post.int32(r));
                if let (Some(reason), Some(port)) = (
                    *self.unique_id.lock(),
                    self.attributes.as_ref().and_then(|a| a.0.port()),
                ) {
                    *self.seen.lock() = port.read_int32_blocking(reason, 0).ok();
                }
                ProcessResult::arrays(vec![Arc::clone(array)])
            }
            fn plugin_type(&self) -> &str {
                "Poster"
            }
            fn register_params(&mut self, base: &mut PortDriverBase) -> Result<(), AsynError> {
                *self.count.lock() =
                    base.create_param("COUNT", asyn_rs::param::ParamType::Int32)?;
                Ok(())
            }
            fn bind_attributes(&mut self, attributes: PluginAttributes) {
                self.attributes = Some(attributes);
            }
        }

        let (downstream_sender, mut downstream_rx) = ndarray_channel("POSTED", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);
        let poster = Poster::default();
        let count = poster.count.clone();
        let (unique_id, seen, read) = (
            poster.unique_id.clone(),
            poster.seen.clone(),
            poster.read.clone(),
        );
        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("POSTER").unwrap(),
            poster,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);
        let port = handle.port_runtime().port_handle();
        let count = *count.lock();
        *unique_id.lock() = Some(handle.ndarray_params.unique_id);
        send_array(handle.array_sender(), make_test_array(41));
        downstream_rx.blocking_recv().unwrap();
        assert_eq!(port.read_int32_blocking(count, 0).unwrap(), 7);
        // C's beginProcessCallbacks has set UniqueId when the callback
        // posts, so the post carries it (NDPluginDriver.cpp:216-232).
        assert_eq!(*seen.lock(), Some(41));
        assert_eq!(
            *read.lock(),
            Some(41),
            "C getIntegerParam reads the begin sets"
        );
    }

    /// A frame the processor leaves uncounted sets ArrayCounter back to what
    /// it was, as NDPluginFile's processCallbacks does (NDPluginFile.cpp:
    /// 727-731, :789).
    #[test]
    fn an_uncounted_frame_leaves_array_counter_alone() {
        struct EvenOnly;
        impl NDPluginProcess for EvenOnly {
            const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
            const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
            const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
            fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
                let mut result = ProcessResult::forward(array, vec![]);
                result.counted = array.unique_id % 2 == 0;
                result
            }
            fn plugin_type(&self) -> &str {
                "EvenOnly"
            }
        }
        let (downstream_sender, mut downstream_rx) = ndarray_channel("EVEN_DOWN", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);
        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("EVENONLY").unwrap(),
            EvenOnly,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);
        for id in 1..=4 {
            send_array(handle.array_sender(), make_test_array(id));
            downstream_rx.blocking_recv().unwrap();
        }
        params_applied(&handle);
        let port = handle.port_runtime().port_handle();
        assert_eq!(
            port.read_int32_blocking(handle.ndarray_params.array_counter, 0)
                .unwrap(),
            2
        );
    }

    /// A processor's array update and a param it sets beside it; `flush`
    /// calls `callParamCallbacks()` between the two.
    #[derive(Default)]
    struct ArrayFrame {
        ids: Arc<parking_lot::Mutex<(usize, usize, usize)>>,
        flush: bool,
    }

    impl ArrayFrame {
        fn updates(&self, other: ParamUpdate, series: ParamUpdate) -> Vec<ParamUpdate> {
            let flush = self.flush.then(|| ParamUpdate::callbacks(0));
            [Some(other), flush, Some(series)]
                .into_iter()
                .flatten()
                .collect()
        }
    }

    impl NDPluginProcess for ArrayFrame {
        const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
        const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
        fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
            let (_, other, series) = *self.ids.lock();
            ProcessResult::forward(
                array,
                self.updates(
                    ParamUpdate::int32(other, 5),
                    ParamUpdate::float64_array(series, vec![1.0, 2.0]),
                ),
            )
        }
        fn plugin_type(&self) -> &str {
            "ArrayFrame"
        }
        fn register_params(&mut self, base: &mut PortDriverBase) -> Result<(), AsynError> {
            let go = base.create_param("GO", asyn_rs::param::ParamType::Int32)?;
            let other = base.create_param("OTHER", asyn_rs::param::ParamType::Int32)?;
            let series = base.create_param("SERIES", asyn_rs::param::ParamType::Float64Array)?;
            *self.ids.lock() = (go, other, series);
            Ok(())
        }
        fn validate_int32(
            &self,
            reason: usize,
            _addr: i32,
            _value: i32,
            verdict: WriteVerdict<i32>,
            _params: &mut WritePort<'_>,
        ) -> WriteOutcome<i32> {
            let (go, other, series) = *self.ids.lock();
            if reason != go {
                return verdict.into();
            }
            WriteOutcome::from(verdict).set_updates(self.updates(
                ParamUpdate::int32(other, 6),
                ParamUpdate::float64_array(series, vec![3.0]),
            ))
        }
    }

    type Posts = Vec<(usize, Option<asyn_rs::interfaces::InterfaceType>)>;

    /// What a frame through an [`ArrayFrame`] posts of NDDimensions, its
    /// series, ArrayCounter, its other param and ExecutionTime, in order.
    fn array_frame_posts(port: &str, flush: bool) -> (Posts, [usize; 5]) {
        let processor = ArrayFrame {
            flush,
            ..ArrayFrame::default()
        };
        let ids = processor.ids.clone();
        let (down, mut down_rx) = ndarray_channel(&format!("{port}_DOWN"), 10);
        let mut output = NDArrayOutput::new();
        output.add(down);
        let claim = asyn_rs::asyn_record::claim_port(port).unwrap();
        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &claim,
            processor,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        // An IOC past iocInit: the frame's callParamCallbacks posts.
        claim
            .publish(
                handle.port_runtime().port_handle().clone(),
                &epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened(),
            )
            .unwrap();
        enable_callbacks(&handle);
        let (_, other, series) = *ids.lock();
        let reasons = [
            handle.ndarray_params.array_dimensions,
            series,
            handle.ndarray_params.array_counter,
            other,
            handle.plugin_params.execution_time,
        ];
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let log = seen.clone();
        let _sync = handle
            .port_runtime()
            .port_handle()
            .interrupts()
            .register_sync_callback(asyn_rs::interrupt::InterruptFilter::default(), move |iv| {
                if reasons.contains(&iv.reason) {
                    log.lock().push((iv.reason, iv.iface))
                }
            })
            .unwrap();
        send_array(handle.array_sender(), make_test_array(1));
        down_rx.blocking_recv().unwrap();
        params_applied(&handle);
        // The flush is enqueued, not run: a round trip runs behind it.
        handle
            .port_runtime()
            .port_handle()
            .read_int32_blocking(reasons[2], 0)
            .unwrap();
        let posts = seen.lock().clone();
        (posts, reasons)
    }

    /// C `beginProcessCallbacks`' `doCallbacksInt32Array(NDDimensions)`
    /// (NDPluginDriver.cpp:246) and a processor's `doCallbacksFloat64Array`
    /// post typed where C calls them, ahead of the frame's
    /// `callParamCallbacks`, and store nothing (asynPortDriver.cpp:1829-1866).
    /// ExecutionTime is set once processCallbacks has returned (:537), and a
    /// list posts in the order its params were set (asynPortDriver.cpp:841).
    #[test]
    fn a_frame_posts_its_arrays_typed_ahead_of_its_params() {
        use asyn_rs::interfaces::InterfaceType;
        let (posts, [dims, series, counter, other, time]) = array_frame_posts("ARRFRAME", false);
        assert_eq!(
            posts,
            [
                (dims, Some(InterfaceType::Int32Array)),
                (series, Some(InterfaceType::Float64Array)),
                (counter, None),
                (other, None),
                (time, None),
            ]
        );
    }

    /// A processor's `callParamCallbacks()` posts where it stands, ahead of
    /// the array it posts next, and ExecutionTime is set only once
    /// processCallbacks has returned (NDPluginDriver.cpp:534-539).
    #[test]
    fn a_frame_flushes_where_its_processor_calls_callbacks() {
        use asyn_rs::interfaces::InterfaceType;
        let (posts, [dims, series, counter, other, time]) = array_frame_posts("ARRFLUSH", true);
        assert_eq!(
            posts,
            [
                (dims, Some(InterfaceType::Int32Array)),
                (counter, None),
                (other, None),
                (series, Some(InterfaceType::Float64Array)),
                (time, None),
            ]
        );
    }

    /// What a write of an [`ArrayFrame`]'s GO posts of GO, its other param
    /// and its series, in order.
    fn array_write_posts(flush: bool) -> (Posts, [usize; 3]) {
        let wiring = test_wiring();
        wiring.register_output(
            "UP",
            Arc::new(parking_lot::Mutex::new(NDArrayOutput::new())),
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let processor = ArrayFrame {
            flush,
            ..ArrayFrame::default()
        };
        let ids = processor.ids.clone();
        let (mut drv, _, _) = PluginPortDriver::new(
            "ARRWRITE",
            "ArrayFrame",
            10,
            0,
            &"UP".into(),
            tx,
            processor,
            None,
            1_000_000,
            wiring,
            PLUGIN_MAX_THREADS,
        )
        .unwrap();
        drv.base
            .own_interrupts(epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened());
        let (go, other, series) = *ids.lock();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let log = seen.clone();
        let _sync = drv
            .base
            .interrupts
            .register_sync_callback(asyn_rs::interrupt::InterruptFilter::default(), move |iv| {
                if [go, other, series].contains(&iv.reason) {
                    log.lock().push((iv.reason, iv.iface))
                }
            })
            .unwrap();
        drv.io_write_int32(&mut AsynUser::new(go), 1).unwrap();
        let posts = seen.lock().clone();
        (posts, [go, other, series])
    }

    /// A write's `doCallbacksFloat64Array` posts typed where it stands, ahead
    /// of the write's `callParamCallbacks`.
    #[test]
    fn a_write_posts_its_array_typed_ahead_of_its_params() {
        use asyn_rs::interfaces::InterfaceType;
        let (posts, [go, other, series]) = array_write_posts(false);
        assert_eq!(
            posts,
            [
                (series, Some(InterfaceType::Float64Array)),
                (go, None),
                (other, None),
            ]
        );
    }

    /// A write's `callParamCallbacks()` in the middle of its work posts where
    /// it stands, as NDPluginTimeSeries' computeNumAverage does (:112).
    #[test]
    fn a_write_flushes_where_it_calls_callbacks() {
        use asyn_rs::interfaces::InterfaceType;
        let (posts, [go, other, series]) = array_write_posts(true);
        assert_eq!(
            posts,
            [
                (go, None),
                (other, None),
                (series, Some(InterfaceType::Float64Array)),
            ]
        );
    }

    /// NDPluginTimeSeries writeInt32 posts its waveforms and arrays from the
    /// write (C `doCallbacksFloat64Array`, `doCallbacksGenericPointer`).
    #[derive(Default)]
    struct ArraySide {
        ids: Arc<parking_lot::Mutex<(usize, usize)>>,
    }

    impl NDPluginProcess for ArraySide {
        const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
        const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
        fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
            ProcessResult::forward(array, vec![])
        }
        fn plugin_type(&self) -> &str {
            "ArraySide"
        }
        fn max_addr(&self) -> usize {
            2
        }
        fn register_params(&mut self, base: &mut PortDriverBase) -> Result<(), AsynError> {
            let go = base.create_param("GO", asyn_rs::param::ParamType::Int32)?;
            let series = base.create_param("SERIES", asyn_rs::param::ParamType::Float64Array)?;
            *self.ids.lock() = (go, series);
            Ok(())
        }
        fn validate_int32(
            &self,
            reason: usize,
            _addr: i32,
            _value: i32,
            verdict: WriteVerdict<i32>,
            _params: &mut WritePort<'_>,
        ) -> WriteOutcome<i32> {
            let (go, series) = *self.ids.lock();
            if reason != go {
                return verdict.into();
            }
            WriteOutcome::from(verdict)
                .set_updates(vec![ParamUpdate::float64_array_addr(
                    series,
                    1,
                    vec![1.0, 2.0],
                )])
                .post_addressed(vec![(1, raw_test_array(42))])
                .settled()
        }
    }

    #[test]
    fn an_array_side_param_is_posted_at_its_address_inside_the_write() {
        let wiring = test_wiring();
        wiring.register_output(
            "UP",
            Arc::new(parking_lot::Mutex::new(NDArrayOutput::new())),
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let processor = ArraySide::default();
        let ids = processor.ids.clone();
        let (mut drv, _, _) = PluginPortDriver::new(
            "ARRSIDE",
            "ArraySide",
            10,
            0,
            &"UP".into(),
            tx,
            processor,
            None,
            1_000_000,
            wiring,
            PLUGIN_MAX_THREADS,
        )
        .unwrap();
        drv.base
            .own_interrupts(epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened());
        let (go, series) = *ids.lock();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let log = seen.clone();
        let _sync = drv
            .base
            .interrupts
            .register_sync_callback(
                asyn_rs::interrupt::InterruptFilter {
                    reason: Some(series),
                    ..Default::default()
                },
                move |iv| {
                    if let ParamValue::Float64Array(v) = &iv.value {
                        log.lock().push((iv.addr, v.to_vec()))
                    }
                },
            )
            .unwrap();
        while rx.try_recv().is_ok() {}
        drv.io_write_int32(&mut AsynUser::new(go), 1).unwrap();
        assert_eq!(*seen.lock(), vec![(1, vec![1.0, 2.0])]);
        // A settled write sends only its addressed post to the data task.
        match rx.try_recv() {
            Ok(PluginParamMsg::Publish(published, addressed)) => {
                assert!(matches!(published, Published::Nothing));
                assert_eq!(addressed.len(), 1);
                assert_eq!((addressed[0].0, addressed[0].1.unique_id), (1, 42));
            }
            other => panic!("expected the addressed post, got {other:?}"),
        }
        assert!(rx.try_recv().is_err());
    }

    /// C's plugin hands `NDPluginDriver` its own `maxAddr`
    /// (NDPluginDriver.cpp:85,90), so both entries build that many addresses.
    #[test]
    fn the_port_has_the_processors_max_addr() {
        struct Three;
        impl NDPluginProcess for Three {
            const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
            const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
            const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
            fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
                ProcessResult::forward(array, vec![])
            }
            fn plugin_type(&self) -> &str {
                "Three"
            }
            fn max_addr(&self) -> usize {
                3
            }
        }
        let wiring = test_wiring();
        let (plain, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("THREE_PLAIN").unwrap(),
            Three,
            1_000_000,
            1,
            0,
            "",
            wiring.clone(),
            DEFAULT_PLUGIN_THREADS,
        );
        let (wired, _jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("THREE_WIRED").unwrap(),
            Three,
            1_000_000,
            1,
            0,
            NDArrayOutput::new(),
            "",
            wiring.clone(),
            DEFAULT_PLUGIN_THREADS,
        );
        for (handle, port) in [(&plain, "THREE_PLAIN"), (&wired, "THREE_WIRED")] {
            assert_eq!(handle.port_runtime().port_handle().max_addr(), 3, "{port}");
            // A single-address port resolves every address to its one output.
            let at = |addr| wiring.output_at(port, addr).unwrap();
            assert!(!Arc::ptr_eq(&at(0), &at(2)), "{port}");
        }
    }

    #[test]
    fn an_array_a_write_posts_at_an_address_reaches_only_that_address() {
        let wiring = test_wiring();
        let processor = ArraySide::default();
        let ids = processor.ids.clone();
        let (up, _up_jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("ARRSIDE_MULTI").unwrap(),
            processor,
            1_000_000,
            10,
            0,
            "",
            wiring.clone(),
            DEFAULT_PLUGIN_THREADS,
        );
        let (down0, rx0) = ndarray_channel("ARRSIDE_DOWN0", 10);
        let (down1, mut rx1) = ndarray_channel("ARRSIDE_DOWN1", 10);
        wiring.subscribe(&down0, "ARRSIDE_MULTI", 0).unwrap();
        wiring.subscribe(&down1, "ARRSIDE_MULTI", 1).unwrap();
        let (go, _) = *ids.lock();
        up.port_runtime()
            .port_handle()
            .write_int32_blocking(go, 0, 1)
            .unwrap();
        assert_eq!(rx1.blocking_recv().unwrap().unique_id, 42);
        params_applied(&up);
        assert_eq!(rx0.pending(), 0);
    }

    /// NDPluginFile.cpp:808-811: a busy write posts its 1 before the work
    /// and its 0 after, so the record sees 1 -> 0 even when the param held
    /// 0 before the write.
    #[test]
    fn a_busy_write_posts_its_value_before_the_work_and_its_reset_after() {
        let wiring = test_wiring();
        wiring.register_output(
            "UP",
            Arc::new(parking_lot::Mutex::new(NDArrayOutput::new())),
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let processor = ValidatingProcessor::default();
        let ids = processor.ids.clone();
        let (mut drv, _, _) = PluginPortDriver::new(
            "BUSY",
            "Validating",
            10,
            0,
            &"UP".into(),
            tx,
            processor,
            None,
            1_000_000,
            wiring,
            PLUGIN_MAX_THREADS,
        )
        .unwrap();
        drv.base
            .own_interrupts(epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened());
        let (limit, ..) = *ids.lock();
        drv.base.set_int32_param(limit, 0, 0).unwrap();
        drv.base.call_param_callbacks(0).unwrap();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let log = seen.clone();
        let _sync = drv
            .base
            .interrupts
            .register_sync_callback(
                asyn_rs::interrupt::InterruptFilter {
                    reason: Some(limit),
                    ..Default::default()
                },
                move |iv| {
                    if let ParamValue::Int32(v) = iv.value {
                        log.lock().push(v)
                    }
                },
            )
            .unwrap();
        drv.io_write_int32(&mut AsynUser::new(limit), 6).unwrap();
        drv.base.call_param_callbacks(0).unwrap();
        assert_eq!(*seen.lock(), vec![6, 0]);
    }

    /// NDPluginDriver.cpp:832 stores `(char *)value`: the text before the
    /// first NUL.
    #[test]
    fn a_plugin_octet_write_stores_up_to_the_first_nul() {
        let wiring = test_wiring();
        wiring.register_output(
            "UP",
            Arc::new(parking_lot::Mutex::new(NDArrayOutput::new())),
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut drv, _, _) = PluginPortDriver::new(
            "OCTETNUL",
            "Passthrough",
            10,
            0,
            &"UP".into(),
            tx,
            PassthroughProcessor,
            None,
            1_000_000,
            wiring,
            PLUGIN_MAX_THREADS,
        )
        .unwrap();
        let name = drv.ndarray_params.file_name;
        drv.io_write_octet(&mut AsynUser::new(name), b"img\0tail")
            .unwrap();
        assert_eq!(drv.base.get_string_param(name, 0).unwrap(), b"img");
    }

    /// A multi-address plugin whose writes set STATUS at address 0, as
    /// ROIStat's TSControl sets TSAcquiring, ending each int32/float64 write
    /// in `EPILOGUE`.
    #[derive(Default)]
    struct AddrZeroStatus<const EPILOGUE: u8> {
        ids: Arc<parking_lot::Mutex<(usize, usize, usize, usize)>>,
    }

    const fn epilogue(e: u8) -> WriteEpilogue {
        match e {
            0 => WriteEpilogue::Addr,
            1 => WriteEpilogue::AddrAndZero,
            _ => WriteEpilogue::Zero,
        }
    }

    impl<const EPILOGUE: u8> NDPluginProcess for AddrZeroStatus<EPILOGUE> {
        const ASYN_FLAGS: PortFlags = ASYN_MULTIDEVICE;
        const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
        const WRITE_INT32_EPILOGUE: WriteEpilogue = epilogue(EPILOGUE);
        const WRITE_FLOAT64_EPILOGUE: WriteEpilogue = epilogue(EPILOGUE);
        fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
            ProcessResult::forward(array, vec![])
        }
        fn plugin_type(&self) -> &str {
            "AddrZeroStatus"
        }
        fn max_addr(&self) -> usize {
            2
        }
        fn register_params(&mut self, base: &mut PortDriverBase) -> Result<(), AsynError> {
            use asyn_rs::param::ParamType;
            *self.ids.lock() = (
                base.create_param("CONTROL", ParamType::Int32)?,
                base.create_param("RATE", ParamType::Float64)?,
                base.create_param("LABEL", ParamType::Octet)?,
                base.create_param("STATUS", ParamType::Int32)?,
            );
            Ok(())
        }
        fn validate_int32(
            &self,
            _reason: usize,
            _addr: i32,
            value: i32,
            verdict: WriteVerdict<i32>,
            _params: &mut WritePort<'_>,
        ) -> WriteOutcome<i32> {
            WriteOutcome::from(verdict).set_int32(self.ids.lock().3, 0, value)
        }
        fn validate_float64(
            &self,
            _reason: usize,
            _addr: i32,
            value: f64,
            verdict: WriteVerdict<f64>,
            _params: &PortDriverBase,
        ) -> WriteOutcome<f64> {
            WriteOutcome::from(verdict).set_int32(self.ids.lock().3, 0, value as i32)
        }
        fn validate_octet(
            &self,
            _reason: usize,
            _addr: i32,
            _value: &str,
            verdict: WriteVerdict<String>,
            _params: &PortDriverBase,
        ) -> WriteOutcome<String> {
            WriteOutcome::from(verdict).set_int32(self.ids.lock().3, 0, 99)
        }
    }

    /// Writes CONTROL 1, RATE 2.0 and LABEL at address 1 of an
    /// `AddrZeroStatus<EPILOGUE>`, with STATUS 7 set but not posted at
    /// address 1 first; returns the STATUS posts seen at address 0, and at 1
    /// before the octet write, which posts address 1 itself.
    fn status_posts<const EPILOGUE: u8>(port: &str) -> (Vec<i32>, Vec<i32>) {
        let wiring = test_wiring();
        wiring.register_output(
            "UP",
            Arc::new(parking_lot::Mutex::new(NDArrayOutput::new())),
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let processor = AddrZeroStatus::<EPILOGUE>::default();
        let ids = processor.ids.clone();
        let (mut drv, _, _) = PluginPortDriver::new(
            port,
            "AddrZeroStatus",
            10,
            0,
            &"UP".into(),
            tx,
            processor,
            None,
            1_000_000,
            wiring,
            PLUGIN_MAX_THREADS,
        )
        .unwrap();
        drv.base
            .own_interrupts(epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened());
        let (control, rate, label, status) = *ids.lock();
        let seen = [0, 1].map(|addr| {
            let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let log = seen.clone();
            let sync = drv
                .base
                .interrupts
                .register_sync_callback(
                    asyn_rs::interrupt::InterruptFilter {
                        reason: Some(status),
                        addr: Some(addr),
                        ..Default::default()
                    },
                    move |iv| {
                        if let ParamValue::Int32(v) = iv.value {
                            log.lock().push(v)
                        }
                    },
                )
                .unwrap();
            (seen, sync)
        });
        drv.base.set_int32_param(status, 1, 7).unwrap();
        drv.io_write_int32(&mut AsynUser::new(control).with_addr(1), 1)
            .unwrap();
        drv.io_write_float64(&mut AsynUser::new(rate).with_addr(1), 2.0)
            .unwrap();
        let [(zero, _), (one, _)] = seen;
        let one = one.lock().clone();
        drv.io_write_octet(&mut AsynUser::new(label).with_addr(1), b"x")
            .unwrap();
        let zero = zero.lock().clone();
        (zero, one)
    }

    /// NDPluginROIStat.cpp:389-390: `callParamCallbacks(roi)` then
    /// `callParamCallbacks()`; a writeOctet posts its address only
    /// (NDPluginDriver.cpp:845).
    #[test]
    fn an_addr_and_zero_write_at_another_address_posts_address_zero_too() {
        assert_eq!(status_posts::<1>("EPIADDRZERO"), (vec![1, 2], vec![7]));
    }

    /// NDPluginDriver.cpp:767, :809: `callParamCallbacks(addr)` leaves an
    /// address-0 param set by the write unposted.
    #[test]
    fn an_addr_only_write_leaves_address_zero_unposted() {
        assert_eq!(status_posts::<0>("EPIADDR"), (vec![], vec![7]));
    }

    /// NDPluginAttribute.cpp:136: `callParamCallbacks()` alone leaves the
    /// written address unposted.
    #[test]
    fn a_zero_only_write_leaves_the_written_address_unposted() {
        assert_eq!(status_posts::<2>("EPIZERO"), (vec![1, 2], vec![]));
    }

    /// Writes 1 to the base-class reason `write` at address 1 of an
    /// `AddrZeroStatus<EPILOGUE>`; returns how often `watch` posted at
    /// addresses 0 and 1 by the time the write returned.
    fn base_write_posts<const EPILOGUE: u8>(
        port: &str,
        pick: impl Fn(&NDArrayDriverParams) -> (usize, usize),
    ) -> [usize; 2] {
        let wiring = test_wiring();
        wiring.register_output(
            "UP",
            Arc::new(parking_lot::Mutex::new(NDArrayOutput::new())),
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut drv, _, _) = PluginPortDriver::new(
            port,
            "AddrZeroStatus",
            10,
            0,
            &"UP".into(),
            tx,
            AddrZeroStatus::<EPILOGUE>::default(),
            None,
            1_000_000,
            wiring,
            PLUGIN_MAX_THREADS,
        )
        .unwrap();
        drv.base
            .own_interrupts(epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened());
        let (write, watch) = pick(&drv.ndarray_params);
        let seen = [0, 1].map(|addr| {
            let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let log = seen.clone();
            let sync = drv
                .base
                .interrupts
                .register_sync_callback(
                    asyn_rs::interrupt::InterruptFilter {
                        reason: Some(watch),
                        addr: Some(addr),
                        ..Default::default()
                    },
                    move |_| {
                        log.fetch_add(1, Ordering::Relaxed);
                    },
                )
                .unwrap();
            (seen, sync)
        });
        drv.io_write_int32(&mut AsynUser::new(write).with_addr(1), 1)
            .unwrap();
        seen.map(|(n, _)| n.load(Ordering::Relaxed))
    }

    /// asynNDArrayDriver.cpp:690-697: PoolPollStats sets the stats at list 0
    /// and posts it before the write returns, on an addr-only plugin too.
    #[test]
    fn a_pool_poll_at_another_address_posts_the_stats_at_address_zero() {
        let posts = base_write_posts::<0>("BASEPOOL", |p| (p.pool_poll_stats, p.pool_max_memory));
        assert_eq!(posts, [1, 0]);
    }

    /// C posts only list 0 (asynNDArrayDriver.cpp:697), leaving the written
    /// address's readback unposted; rs posts it as well.
    #[test]
    fn a_base_write_at_another_address_posts_its_readback() {
        let [_, readback] =
            base_write_posts::<0>("BASEREADBACK", |p| (p.file_number, p.file_number));
        assert_eq!(readback, 1);
    }

    /// NDPluginDriver.cpp:702, :792, :829: parseAsynUser makes a
    /// single-device plugin's address list 0.
    #[test]
    fn a_single_device_plugin_write_at_another_address_is_list_zero() {
        let wiring = test_wiring();
        wiring.register_output(
            "UP",
            Arc::new(parking_lot::Mutex::new(NDArrayOutput::new())),
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut drv, _, _) = PluginPortDriver::new(
            "ADDRLIST0",
            "Passthrough",
            10,
            0,
            &"UP".into(),
            tx,
            PassthroughProcessor,
            None,
            1_000_000,
            wiring,
            PLUGIN_MAX_THREADS,
        )
        .unwrap();
        assert!(!drv.base.flags.multi_device);
        let number = drv.ndarray_params.file_number;
        drv.io_write_int32(&mut AsynUser::new(number).with_addr(1), 4)
            .unwrap();
        assert_eq!(drv.base.get_int32_param(number, 0).unwrap(), 4);
        let rate = drv.plugin_params.max_byte_rate;
        drv.io_write_float64(&mut AsynUser::new(rate).with_addr(1), 2.5)
            .unwrap();
        assert_eq!(drv.base.get_float64_param(rate, 0).unwrap(), 2.5);
        let name = drv.ndarray_params.file_name;
        drv.io_write_octet(&mut AsynUser::new(name).with_addr(1), b"img")
            .unwrap();
        assert_eq!(drv.base.get_string_param(name, 0).unwrap(), b"img");
    }

    /// NDPluginDriver.cpp:702-703, :764-769: a write parseAsynUser refuses
    /// goes to `done:` and prints the ERROR line with getAddress's status.
    #[test]
    fn a_refused_plugin_address_prints_the_write_int32_error_line() {
        let wiring = test_wiring();
        wiring.register_output(
            "UP",
            Arc::new(parking_lot::Mutex::new(NDArrayOutput::new())),
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut drv, _, _) = PluginPortDriver::new(
            "ADDRDONE",
            "Passthrough",
            10,
            0,
            &"UP".into(),
            tx,
            PassthroughProcessor,
            None,
            1_000_000,
            wiring,
            PLUGIN_MAX_THREADS,
        )
        .unwrap();
        drv.base.flags.multi_device = true;
        let log = std::env::temp_dir().join(format!("addr-done-{}.log", std::process::id()));
        let file = std::fs::File::create(&log).unwrap();
        asyn_rs::services::PortServices::global()
            .trace()
            .set_trace_file(
                Some("ADDRDONE"),
                asyn_rs::trace::TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
            );
        let number = drv.ndarray_params.file_number;
        let e = drv
            .io_write_int32(&mut AsynUser::new(number).with_addr(1), 4)
            .unwrap_err();
        let text = std::fs::read_to_string(&log).unwrap();
        let _ = std::fs::remove_file(&log);
        assert_eq!(e.c_status_code(), 3);
        assert!(
            text.contains(&format!(
                "NDPluginDriver::writeInt32 ERROR, status=3, function={number}, paramName=FILE_NUMBER, value=4, connectedToArrayPort_=1"
            )),
            "{text}"
        );
    }

    /// NDPluginDriver.cpp:702-703, :764: `done:` calls back the list getAddress
    /// resolved, so a bad reason on a single-device port flushes list 0
    /// whatever the record's address.
    #[test]
    fn a_refused_plugin_reason_calls_back_the_resolved_list() {
        let wiring = test_wiring();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut drv, _, _) = PluginPortDriver::new(
            "ADDRLIST",
            "Passthrough",
            10,
            0,
            &"UP".into(),
            tx,
            PassthroughProcessor,
            None,
            1_000_000,
            wiring,
            PLUGIN_MAX_THREADS,
        )
        .unwrap();
        assert!(!drv.base.flags.multi_device);
        drv.base
            .own_interrupts(epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened());
        drv.base.params.take_changed(0).unwrap();
        let number = drv.ndarray_params.file_number;
        drv.base.params.mark_changed(number, 0).unwrap();
        let bogus = drv.base.params.len() + 5;
        assert!(
            drv.io_write_int32(&mut AsynUser::new(bogus).with_addr(1), 4)
                .is_err()
        );
        assert_eq!(
            drv.base.params.take_changed(0).unwrap(),
            Vec::<usize>::new()
        );
    }

    /// asynNDArrayDriver.cpp:529-538 inside the write: FilePath gains its
    /// separator, a missing directory is created to CreateDir's depth, and
    /// FilePathExists is set before the write returns. A path left missing
    /// fails the write, which C's status overwrites hide (:540).
    #[test]
    fn a_file_path_write_checks_and_creates_the_path_as_c_write_octet_does() {
        let wiring = test_wiring();
        wiring.register_output(
            "UP",
            Arc::new(parking_lot::Mutex::new(NDArrayOutput::new())),
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut drv, _, _) = PluginPortDriver::new(
            "FILEPATH",
            "Passthrough",
            10,
            0,
            &"UP".into(),
            tx,
            PassthroughProcessor,
            None,
            1_000_000,
            wiring,
            PLUGIN_MAX_THREADS,
        )
        .unwrap();
        let nd = drv.ndarray_params;
        let dir = std::env::temp_dir().join(format!("filepath-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("a").to_str().unwrap().to_string();
        let checked = format!("{path}{}", std::path::MAIN_SEPARATOR);
        let write = |drv: &mut PluginPortDriver| {
            let mut user = AsynUser::new(nd.file_path);
            let status = match drv.io_write_octet(&mut user, path.as_bytes()) {
                Ok(_) => None,
                Err(AsynError::Status { message, .. }) => Some(message),
                Err(e) => panic!("{e}"),
            };
            (
                status,
                String::from_utf8_lossy(drv.base.get_string_param(nd.file_path, 0).unwrap())
                    .into_owned(),
                drv.base.get_int32_param(nd.file_path_exists, 0).unwrap(),
            )
        };

        let refused = format!(
            "NDPluginDriver::writeOctet: status=3, function={}, paramName=FILE_PATH, value={path}",
            nd.file_path
        );
        assert_eq!(
            write(&mut drv),
            (Some(refused), checked.clone(), 0),
            "CreateDir 0 creates nothing"
        );
        drv.base.set_int32_param(nd.create_dir, 0, -2).unwrap();
        assert_eq!(write(&mut drv), (None, checked.clone(), 1));
        assert!(std::path::Path::new(&path).is_dir());
        let mut last = None;
        while let Ok(PluginParamMsg::Change(r, _, v)) = rx.try_recv() {
            last = Some((r, v.as_string().map(str::to_string)));
        }
        assert_eq!(last, Some((nd.file_path, Some(checked))));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// C `createCallbackThreads` reads all three params whichever one was
    /// written (NDPluginDriver.cpp:746-748, :969-978), so a QueueSize write
    /// clamps a NumThreads left above a lowered MaxThreads, and fails for it.
    #[test]
    fn queue_size_write_clamps_num_threads_to_max_threads() {
        let wiring = test_wiring();
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("XCLAMP").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            "",
            wiring,
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        let p = &handle.plugin_params;
        port.write_int32_blocking(p.max_threads, 0, 4).unwrap();
        port.write_int32_blocking(p.num_threads, 0, 3).unwrap();
        port.write_int32_blocking(p.max_threads, 0, 1).unwrap();
        assert_eq!(port.read_int32_blocking(p.num_threads, 0).unwrap(), 3);

        assert!(port.write_int32_blocking(p.queue_size, 0, 7).is_err());
        params_applied(&handle);
        assert_eq!(port.read_int32_blocking(p.queue_size, 0).unwrap(), 7);
        assert_eq!(port.read_int32_blocking(p.queue_use, 0).unwrap(), 7);
        assert_eq!(port.read_int32_blocking(p.num_threads, 0).unwrap(), 1);
    }

    /// What `port`'s trace prints while `f` runs.
    fn traced(port: &str, f: impl FnOnce()) -> String {
        let log = std::env::temp_dir().join(format!("{port}-trace-{}.log", std::process::id()));
        let file = std::fs::File::create(&log).unwrap();
        asyn_rs::services::PortServices::global()
            .trace()
            .set_trace_file(
                Some(port),
                asyn_rs::trace::TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
            );
        f();
        let trace = std::fs::read_to_string(&log).unwrap();
        let _ = std::fs::remove_file(&log);
        trace
    }

    /// C's constructor runs `createCallbackThreads` only for a non-blocking
    /// plugin (NDPluginDriver.cpp:164-166), whoever builds it.
    #[test]
    fn the_constructor_floors_the_queue_only_when_non_blocking() {
        const FLOOR: &str =
            "NDPluginDriver::createCallbackThreads error, queueSize=0 must be >= 1, setting to 1";
        for (port, blocking, size) in [("XCTOR0", 0, 1), ("XCTOR1", 1, 0)] {
            let mut handle = None;
            let trace = traced(port, || {
                handle = Some(create_plugin_runtime(
                    &asyn_rs::asyn_record::claim_port(port).unwrap(),
                    PassthroughProcessor,
                    1_000_000,
                    0,
                    blocking,
                    "",
                    test_wiring(),
                    DEFAULT_PLUGIN_THREADS,
                ));
            });
            let (handle, _jh) = handle.unwrap();
            let port_handle = handle.port_runtime().port_handle();
            let p = &handle.plugin_params;
            let read = |r| port_handle.read_int32_blocking(r, 0).unwrap();
            assert_eq!(read(p.blocking_callbacks), blocking, "{port}");
            assert_eq!(
                (read(p.queue_size), read(p.queue_use)),
                (size, size),
                "{port}"
            );
            assert_eq!(trace.contains(FLOOR), blocking == 0, "{port}:\n{trace}");
        }
    }

    /// Once the threads exist, a BlockingCallbacks 1 then 0 does not run
    /// `createCallbackThreads` again (`:715-717` tests `pThreads_.size()`),
    /// so a NumThreads above a since-lowered MaxThreads stands.
    #[test]
    fn going_blocking_and_back_does_not_rerun_create_callback_threads() {
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("XRERUN").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        let p = &handle.plugin_params;
        port.write_int32_blocking(p.max_threads, 0, 4).unwrap();
        port.write_int32_blocking(p.num_threads, 0, 4).unwrap();
        port.write_int32_blocking(p.max_threads, 0, 2).unwrap();
        let trace = traced("XRERUN", || {
            port.write_int32_blocking(p.blocking_callbacks, 0, 1)
                .unwrap();
            port.write_int32_blocking(p.blocking_callbacks, 0, 0)
                .unwrap();
        });
        assert_eq!(port.read_int32_blocking(p.num_threads, 0).unwrap(), 4);
        assert!(!trace.contains("createCallbackThreads"), "{trace}");
    }

    #[test]
    fn refused_plugin_writes_return_an_error() {
        let wiring = test_wiring();
        let up_output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        wiring.register_output("UP", up_output);
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("REFUSE").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            "UP",
            wiring.clone(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        let p = &handle.plugin_params;
        let log = std::env::temp_dir().join(format!("refuse-trace-{}.log", std::process::id()));
        let file = std::fs::File::create(&log).unwrap();
        port.trace().set_trace_file(
            Some("REFUSE"),
            asyn_rs::trace::TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );

        assert!(port.write_int32_blocking(p.num_threads, 0, 2).is_err());
        assert_eq!(port.read_int32_blocking(p.num_threads, 0).unwrap(), 1);
        assert!(port.write_int32_blocking(p.num_threads, 0, 0).is_err());
        assert!(port.write_int32_blocking(p.num_threads, 0, 1).is_ok());

        // C `writeInt32` has no MaxThreads arm: lowering the ceiling leaves
        // the accepted NumThreads alone.
        port.write_int32_blocking(p.max_threads, 0, 4).unwrap();
        port.write_int32_blocking(p.num_threads, 0, 3).unwrap();
        port.write_int32_blocking(p.max_threads, 0, 1).unwrap();
        params_applied(&handle);
        assert_eq!(port.read_int32_blocking(p.num_threads, 0).unwrap(), 3);
        assert_eq!(port.read_int32_blocking(p.max_threads, 0).unwrap(), 1);

        assert!(port.write_int32_blocking(p.queue_size, 0, 0).is_err());
        assert_eq!(port.read_int32_blocking(p.queue_size, 0).unwrap(), 1);
        assert_eq!(port.read_int32_blocking(p.queue_use, 0).unwrap(), 1);
        port.write_int32_blocking(p.queue_size, 0, 7).unwrap();
        assert_eq!(port.read_int32_blocking(p.queue_use, 0).unwrap(), 7);

        // A single-address upstream takes any NDArrayAddr.
        assert!(port.write_int32_blocking(p.nd_array_addr, 0, 3).is_ok());
        assert!(port.write_int32_blocking(p.nd_array_addr, 0, 0).is_ok());

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // An NDArrayPort write fails as an NDArrayAddr one does, which C's
        // writeOctet hides by dropping connectToArrayPort's status (:836).
        match rt.block_on(port.write_octet(p.nd_array_port, 0, b"NOPE".to_vec())) {
            Err(AsynError::Status { message, .. }) => assert_eq!(
                message,
                format!(
                    "NDPluginDriver::writeOctet: status=3, function={}, paramName=NDARRAY_PORT, value=NOPE",
                    p.nd_array_port
                )
            ),
            other => panic!("{other:?}"),
        }
        let (data, status) = rt
            .block_on(port.read_octet(p.nd_array_port, 0, 16))
            .into_parts();
        status.unwrap();
        assert_eq!(data, b"NOPE\0");
        // A multi-device upstream takes an address it publishes nothing at,
        // as `locateDevice` allocates one (asynManager.c:571-589).
        for addr in 0..3 {
            let output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
            wiring.register_output_at("UPM", addr, output);
        }
        assert!(
            rt.block_on(port.write_octet(p.nd_array_port, 0, b"UPM".to_vec()))
                .is_ok()
        );
        assert!(port.write_int32_blocking(p.nd_array_addr, 0, 5).is_ok());
        assert!(port.write_int32_blocking(p.nd_array_addr, 0, -1).is_ok());
        assert!(port.write_int32_blocking(p.nd_array_addr, 0, 0).is_ok());
        // An asyn port without NDArrays connects but has no interface.
        asyn_rs::registry::claim_port("REFUSE_NOGP")
            .unwrap()
            .publish(port.clone(), &asyn_rs::registry::open_owner())
            .unwrap();
        assert!(
            rt.block_on(port.write_octet(p.nd_array_port, 0, b"REFUSE_NOGP".to_vec()))
                .is_err()
        );
        assert!(port.write_int32_blocking(p.nd_array_addr, 0, 2).is_err());
        asyn_rs::registry::unregister_port("REFUSE_NOGP");
        assert!(
            rt.block_on(port.write_octet(p.nd_array_port, 0, b"UP".to_vec()))
                .is_ok()
        );

        // The lines C prints for the same writes (NDPluginDriver.cpp:606,
        // :614-618, :973-990, :769-771).
        let trace = std::fs::read_to_string(&log).unwrap();
        let _ = std::fs::remove_file(&log);
        for line in [
            "NDPluginDriver::createCallbackThreads error, numThreads=2 must be <= maxThreads=1, setting to 1".to_string(),
            format!(
                "NDPluginDriver::writeInt32 ERROR, status=3, function={}, paramName=NUM_THREADS, value=2, connectedToArrayPort_=1",
                p.num_threads
            ),
            "NDPluginDriver::createCallbackThreads error, numThreads=0 must be >= 1, setting to 1".to_string(),
            "NDPluginDriver::createCallbackThreads error, queueSize=0 must be >= 1, setting to 1".to_string(),
            "NDPluginDriver::connectToArrayPort Error calling pasynManager->connectDevice to array port NOPE address 0, status=3, error=asynManager:connectDevice port NOPE not found".to_string(),
            "NDPluginDriver::connectToPort ERROR: Can't find asynGenericPointer interface on array port REFUSE_NOGP address 0".to_string(),
            "NDPluginDriver::connectToPort ERROR: Can't find asynGenericPointer interface on array port REFUSE_NOGP address 2".to_string(),
        ] {
            assert!(trace.contains(&line), "missing {line:?} in\n{trace}");
        }
        for absent in ["UPM", "address 5", "address -1"] {
            assert!(!trace.contains(absent), "{absent:?} refused in\n{trace}");
        }
    }

    #[test]
    fn arrays_reach_only_the_subscribers_of_their_address() {
        // C doCallbacksGenericPointer(pArray, NDArrayData, addr) calls only the
        // clients registered at `addr` (NDPluginStats.cpp:586 posts its time
        // series at 1, endProcessCallbacks posts the image at 0), and a post
        // made outside endProcessCallbacks ignores NDArrayCallbacks.
        struct Posting;
        impl NDPluginProcess for Posting {
            const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
            const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
            const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
            fn max_addr(&self) -> usize {
                2
            }
            fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
                let mut result = ProcessResult::forward(array, vec![]);
                result
                    .addressed
                    .push((1, raw_test_array(array.unique_id + 1000)));
                result
            }
            fn plugin_type(&self) -> &str {
                "Posting"
            }
        }
        let wiring = test_wiring();
        let (up, _up_jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("UP_MULTI").unwrap(),
            Posting,
            1_000_000,
            10,
            0,
            "",
            wiring.clone(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&up);
        let (down0, mut rx0) = ndarray_channel("DOWN0", 10);
        let (down1, mut rx1) = ndarray_channel("DOWN1", 10);
        wiring.subscribe(&down0, "UP_MULTI", 0).unwrap();
        wiring.subscribe(&down1, "UP_MULTI", 1).unwrap();

        // Each consumer's next array proves the previous one never crossed.
        for id in [99, 100] {
            send_array(up.array_sender(), make_test_array(id));
            assert_eq!(rx1.blocking_recv().unwrap().unique_id, id + 1000);
            assert_eq!(rx0.blocking_recv().unwrap().unique_id, id);
        }

        let port = up.port_runtime().port_handle();
        port.write_int32_blocking(up.ndarray_params.array_callbacks, 0, 0)
            .unwrap();
        params_applied(&up);
        send_array(up.array_sender(), make_test_array(101));
        assert_eq!(rx1.blocking_recv().unwrap().unique_id, 1101);
        port.write_int32_blocking(up.ndarray_params.array_callbacks, 0, 1)
            .unwrap();
        params_applied(&up);
        send_array(up.array_sender(), make_test_array(102));
        assert_eq!(rx1.blocking_recv().unwrap().unique_id, 1102);
        assert_eq!(
            rx0.blocking_recv().unwrap().unique_id,
            102,
            "NDArrayCallbacks=0 withheld 101 from address 0 only"
        );
    }

    /// C's `this->pNDArrayPool = pArray->pNDArrayPool` (NDPluginDriver.cpp:419):
    /// a plugin allocates from the pool of the array it was handed, not the
    /// one it was configured with, so its output counts on that pool's driver.
    #[test]
    fn a_plugin_allocates_from_the_incoming_arrays_pool() {
        struct Allocating;
        impl NDPluginProcess for Allocating {
            const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
            const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
            const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
            fn process_array(&self, array: &Arc<NDArray>, pool: &NDArrayPool) -> ProcessResult {
                let out = pool.alloc(array.dims.clone(), NDDataType::UInt8).unwrap();
                ProcessResult::arrays(vec![Arc::new(out)])
            }
            fn plugin_type(&self) -> &str {
                "Allocating"
            }
        }
        let detector_pool = DriverPool::standalone(1_000_000);
        let wiring = test_wiring();
        let (plugin, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("ADOPT_POOL").unwrap(),
            Allocating,
            1_000_000,
            10,
            0,
            "",
            wiring.clone(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&plugin);
        let (down, mut rx) = ndarray_channel("ADOPT_DOWN", 10);
        wiring.subscribe(&down, "ADOPT_POOL", 0).unwrap();

        let frame = detector_pool
            .alloc(vec![NDDimension::new(4)], NDDataType::UInt8)
            .unwrap();
        send_array(plugin.array_sender(), frame.share());
        let out = rx.blocking_recv().unwrap();
        assert_eq!(out.pool_id(), detector_pool.id());
    }

    #[test]
    fn a_processor_gates_its_work_on_the_runtimes_ndarray_callbacks() {
        // C NDPluginTimeSeries does its per-address work inside its own
        // `if (arrayCallbacks)` (NDPluginTimeSeries.cpp:341), from a frame or
        // a write alike; an ungated post (NDPluginStats.cpp:586) ignores it.
        #[derive(Default)]
        struct Gated {
            array_callbacks: Option<ArrayCallbacks>,
            gated_frames: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl Gated {
            fn on(&self) -> bool {
                self.array_callbacks.as_ref().unwrap().get()
            }
        }
        impl NDPluginProcess for Gated {
            const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
            const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
            const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
            fn max_addr(&self) -> usize {
                2
            }
            fn bind_array_callbacks(&mut self, array_callbacks: ArrayCallbacks) {
                self.array_callbacks = Some(array_callbacks);
            }
            fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
                let mut result = ProcessResult::sink(vec![]);
                result
                    .addressed
                    .push((1, raw_test_array(array.unique_id + 1000)));
                if self.on() {
                    self.gated_frames.fetch_add(1, Ordering::Relaxed);
                    result
                        .addressed
                        .push((1, raw_test_array(array.unique_id + 2000)));
                }
                result
            }
            fn on_param_change(
                &self,
                _reason: usize,
                params: &PluginParamSnapshot,
            ) -> ParamChangeResult {
                let mut result = ParamChangeResult::empty();
                if self.on() {
                    result
                        .addressed
                        .push((1, raw_test_array(5000 + params.value.as_i32())));
                }
                result
            }
            fn plugin_type(&self) -> &str {
                "Gated"
            }
        }
        let gated = Gated::default();
        let gated_frames = gated.gated_frames.clone();
        let wiring = test_wiring();
        let (up, _up_jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("UP_GATED").unwrap(),
            gated,
            1_000_000,
            10,
            0,
            "",
            wiring.clone(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&up);
        let (down1, mut rx1) = ndarray_channel("DOWN_GATED", 10);
        wiring.subscribe(&down1, "UP_GATED", 1).unwrap();
        let port = up.port_runtime().port_handle();
        let mut next = || rx1.blocking_recv().unwrap().unique_id;

        send_array(up.array_sender(), make_test_array(99));
        assert_eq!((next(), next()), (1099, 2099));
        port.write_int32_blocking(up.ndarray_params.array_callbacks, 0, 0)
            .unwrap();
        params_applied(&up);
        send_array(up.array_sender(), make_test_array(101));
        // The gate is read when the queued frame is processed, as C's
        // processCallbacks reads arrayCallbacks on the plugin thread, so 101
        // must be out before the write of 1.
        assert_eq!(next(), 1101);
        port.write_int32_blocking(up.ndarray_params.array_callbacks, 0, 1)
            .unwrap();
        params_applied(&up);
        send_array(up.array_sender(), make_test_array(102));
        // The write of 0 already sees 0, so 5000 and 2101 were never made.
        assert_eq!((next(), next(), next()), (5001, 1102, 2102));
        assert_eq!(gated_frames.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn the_constructor_source_address_is_seeded_and_connected() {
        // C seeds NDArrayAddr from the constructor argument (:153) and the
        // plugin constructor connects there; an unseeded 0 is what the
        // NDArrayAddress record's init readback then copies over its VAL.
        struct TwoAddresses;
        impl NDPluginProcess for TwoAddresses {
            const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
            const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
            const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
            fn max_addr(&self) -> usize {
                2
            }
            fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
                ProcessResult::forward(array, vec![])
            }
            fn plugin_type(&self) -> &str {
                "TwoAddresses"
            }
        }
        let wiring = test_wiring();
        let (up, _up_jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("UP_SEED").unwrap(),
            TwoAddresses,
            1_000_000,
            10,
            0,
            "",
            wiring.clone(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&up);
        let (down, _down_jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("DOWN_SEED").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            ("UP_SEED", 1),
            wiring.clone(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = down.port_runtime().port_handle();
        assert_eq!(
            port.read_int32_blocking(down.plugin_params.nd_array_addr, 0)
                .unwrap(),
            1
        );
        down.connect_to_array_port(&wiring).unwrap();
        let at_addr1 = wiring.output_at("UP_SEED", 1).expect("UP_SEED:1");
        assert_eq!(at_addr1.lock().num_senders(), 1);
    }

    #[test]
    fn a_plugin_receives_from_an_address_published_after_it_connects() {
        // C's interrupt user is per (port, addr) and needs no callback there
        // yet: a driver's later doCallbacksGenericPointer at that address
        // reaches the plugin.
        let wiring = test_wiring();
        let drv0 = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        wiring.register_output("LATE_DRV", drv0.clone());
        let (downstream_sender, mut downstream_rx) = ndarray_channel("LATE_SINK", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);
        let (down, _down_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("LATE_DOWN").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            output,
            ("LATE_DRV", 2),
            wiring.clone(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&down);
        down.connect_to_array_port(&wiring).unwrap();

        let drv2 = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        wiring.register_output_at("LATE_DRV", 2, drv2.clone());
        assert_eq!(
            drv2.lock().num_senders(),
            1,
            "the publisher adopts the plugin"
        );
        let publisher = crate::plugin::channel::ArrayPublisher::new(drv2);
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(publisher.publish(make_test_array(7)));
        })
        .join()
        .unwrap();
        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 7);
        assert_eq!(drv0.lock().num_senders(), 0);
    }

    #[test]
    fn a_gathering_plugin_wires_one_source_per_address() {
        // NDGatherN.template writes NDArrayPort at address N. A plugin
        // reading `num_array_sources` pairs holds one wiring per slot: slot 1
        // joins a second upstream without moving slot 0, and an empty port
        // at address 1 leaves only slot 0.
        struct Gathering;
        impl NDPluginProcess for Gathering {
            const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
            const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
            const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
            fn max_addr(&self) -> usize {
                2
            }
            fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
                ProcessResult::forward(array, vec![])
            }
            fn plugin_type(&self) -> &str {
                "Gathering"
            }
            fn num_array_sources(&self) -> usize {
                2
            }
        }
        let wiring = test_wiring();
        let mut ups = Vec::new();
        for name in ["UP_A", "UP_B"] {
            let (up, jh) = create_plugin_runtime(
                &asyn_rs::asyn_record::claim_port(name).unwrap(),
                PassthroughProcessor,
                1_000_000,
                10,
                0,
                "",
                wiring.clone(),
                DEFAULT_PLUGIN_THREADS,
            );
            enable_callbacks(&up);
            ups.push((up, jh));
        }
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("GATHERING").unwrap(),
            Gathering,
            1_000_000,
            10,
            0,
            "UP_A",
            wiring.clone(),
            DEFAULT_PLUGIN_THREADS,
        );
        wiring.subscribe(handle.array_sender(), "UP_A", 0).unwrap();
        let (down, mut down_rx) = ndarray_channel("DOWN", 10);
        wiring.subscribe(&down, "GATHERING", 0).unwrap();
        enable_callbacks(&handle);

        let port_reason = handle.plugin_params.nd_array_port;
        let write_port = |addr: i32, port: &str| {
            handle
                .param_tx
                .send(PluginParamMsg::Change(
                    port_reason,
                    addr,
                    ParamChangeValue::Octet(port.to_string()),
                ))
                .unwrap();
            params_applied(&handle);
        };
        write_port(1, "UP_B");
        send_array(ups[1].0.array_sender(), make_test_array(2));
        send_array(ups[0].0.array_sender(), make_test_array(1));
        // UP_A and UP_B forward on their own threads, so the two arrivals
        // carry no order.
        let mut ids = [
            down_rx.blocking_recv().unwrap().unique_id,
            down_rx.blocking_recv().unwrap().unique_id,
        ];
        ids.sort_unstable();
        assert_eq!(ids, [1, 2], "both slots deliver");

        write_port(1, "");
        send_array(ups[1].0.array_sender(), make_test_array(3));
        send_array(ups[0].0.array_sender(), make_test_array(4));
        assert_eq!(
            down_rx.blocking_recv().unwrap().unique_id,
            4,
            "an emptied slot 1 delivers nothing; slot 0 still does"
        );

        // C connectToArrayPort disconnects before it connects
        // (NDPluginDriver.cpp:600-611): a port that does not resolve leaves
        // the slot receiving nothing, and the next good write reconnects it.
        let (probe, mut probe_rx) = ndarray_channel("PROBE", 10);
        wiring.subscribe(&probe, "UP_A", 0).unwrap();
        write_port(0, "NOPE");
        send_array(ups[0].0.array_sender(), make_test_array(5));
        assert_eq!(probe_rx.blocking_recv().unwrap().unique_id, 5);
        write_port(0, "UP_A");
        send_array(ups[0].0.array_sender(), make_test_array(6));
        assert_eq!(
            down_rx.blocking_recv().unwrap().unique_id,
            6,
            "array 5 went out while slot 0 was on NOPE"
        );
    }

    #[test]
    fn test_nonblocking_passthrough() {
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("NB_TEST").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);

        send_array(handle.array_sender(), make_test_array(42));

        let received = downstream_rx.blocking_recv().unwrap();
        assert_eq!(received.unique_id, 42);
    }

    #[test]
    fn test_blocking_to_nonblocking_switch() {
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("SWITCH_TEST").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);

        // Start in blocking mode
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.blocking_callbacks, 0, 1)
            .unwrap();
        params_applied(&handle);

        send_array(handle.array_sender(), make_test_array(1));
        let received = downstream_rx.blocking_recv().unwrap();
        assert_eq!(received.unique_id, 1);

        // Switch back to non-blocking
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.blocking_callbacks, 0, 0)
            .unwrap();
        params_applied(&handle);

        // Send in non-blocking mode — goes through channel to data thread
        send_array(handle.array_sender(), make_test_array(2));
        let received = downstream_rx.blocking_recv().unwrap();
        assert_eq!(received.unique_id, 2);
    }

    /// A plugin built with blocking callbacks holds the caller until its
    /// first frame is processed (NDPluginDriver.cpp:160, :419-422), with no
    /// BlockingCallbacks write after the constructor.
    #[test]
    fn a_blocking_plugin_holds_the_caller_from_its_first_frame() {
        struct Held {
            started: std::sync::mpsc::SyncSender<()>,
            release: parking_lot::Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl NDPluginProcess for Held {
            const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
            const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
            const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
            fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
                let _ = self.started.send(());
                let _ = self
                    .release
                    .lock()
                    .recv_timeout(std::time::Duration::from_secs(10));
                ProcessResult::arrays(vec![Arc::clone(array)])
            }
            fn plugin_type(&self) -> &str {
                "Held"
            }
        }
        let (started_tx, started) = std::sync::mpsc::sync_channel(1);
        let (release, release_rx) = std::sync::mpsc::channel();
        let (handle, _data_jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("BLK_CTOR").unwrap(),
            Held {
                started: started_tx,
                release: parking_lot::Mutex::new(release_rx),
            },
            1_000_000,
            10,
            1,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);

        let returned = Arc::new(AtomicBool::new(false));
        let publisher = {
            let returned = Arc::clone(&returned);
            let sender = handle.array_sender().clone();
            std::thread::spawn(move || {
                send_array(&sender, make_test_array(1));
                returned.store(true, Ordering::Release);
            })
        };
        started
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            !returned.load(Ordering::Acquire),
            "the caller returned while its frame was still processing"
        );
        release.send(()).unwrap();
        publisher.join().unwrap();
    }

    /// A frame `processing_required` refuses is neither counted, posted nor
    /// handed on, on the callback thread or the caller's
    /// (NDPluginFile.cpp:724-725).
    #[test]
    fn a_refused_frame_never_reaches_begin() {
        struct OddOnly;
        impl NDPluginProcess for OddOnly {
            const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
            const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
            const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
            fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
                ProcessResult::arrays(vec![Arc::clone(array)])
            }
            fn plugin_type(&self) -> &str {
                "OddOnly"
            }
            fn processing_required(&self, array: &NDArray) -> bool {
                array.unique_id % 2 == 1
            }
        }
        for (blocking, name) in [(0, "REFUSE_THREAD"), (1, "REFUSE_BLOCKING")] {
            let (ds, mut rx) = ndarray_channel("DS", 10);
            let mut output = NDArrayOutput::new();
            output.add(ds);
            let (handle, _jh) = create_plugin_runtime_with_output(
                &asyn_rs::asyn_record::claim_port(name).unwrap(),
                OddOnly,
                1_000_000,
                10,
                0,
                output,
                "",
                test_wiring(),
                DEFAULT_PLUGIN_THREADS,
            );
            enable_callbacks(&handle);
            let port = handle.port_runtime().port_handle().clone();
            port.write_int32_blocking(handle.plugin_params.blocking_callbacks, 0, blocking)
                .unwrap();
            params_applied(&handle);
            send_array(handle.array_sender(), make_test_array(2));
            send_array(handle.array_sender(), make_test_array(3));
            assert_eq!(rx.blocking_recv().unwrap().unique_id, 3, "{name}");
            wait_until("the processed frame's post", || {
                port.read_int32_blocking(handle.ndarray_params.unique_id, 0)
                    .is_ok_and(|v| v == 3)
            });
            assert_eq!(
                port.read_int32_blocking(handle.ndarray_params.array_counter, 0)
                    .unwrap(),
                1,
                "{name}"
            );
        }
    }

    #[test]
    fn test_enable_callbacks_disables_processing() {
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("ENABLE_TEST").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );

        // Disable callbacks
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 0)
            .unwrap();
        params_applied(&handle);

        // Send array — should be silently dropped by sender (callbacks disabled)
        send_array(handle.array_sender(), make_test_array(99));

        // Verify nothing received (with timeout)
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_millis(100), downstream_rx.recv()).await
        });
        assert!(
            result.is_err(),
            "should not receive array when callbacks disabled"
        );
    }

    /// Disabling a busy plugin does not throw away what it already accepted.
    ///
    /// C's `EnableCallbacks=0` cancels the array interrupt and nothing else
    /// (NDPluginDriver.cpp:711-720), so the callback threads keep emptying
    /// `pToThreadMsgQ_` — `deleteCallbackThreads` depends on precisely that,
    /// disabling the interrupt and then waiting for `pending()` to fall to
    /// zero (`:1026-1034`), which it never would if disabling discarded the
    /// backlog instead.
    ///
    /// The observable is the work: on an NDPluginFile those frames are files
    /// written, on an NDPluginStats they are counts a client reads back.
    #[test]
    fn disabling_callbacks_does_not_discard_the_queued_backlog() {
        const LAST: i32 = 5;
        let (processor, entered, gated) = GatedProcessor::new();
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("DISABLE_MID").unwrap(),
            processor,
            1_000_000,
            8,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        enable_callbacks(&handle);

        // Park the one callback thread on frame 1, then build a backlog
        // behind it — frames the plugin has accepted but not yet worked on.
        send_array(handle.array_sender(), make_test_array(1));
        wait_until("frame 1 reached the callback thread", || {
            entered.load(Ordering::Acquire) == 1
        });
        for id in 2..=LAST {
            send_array(handle.array_sender(), make_test_array(id));
        }
        port.write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 0)
            .unwrap();
        param_fifo_drained(&handle);

        // A frame offered after the disable is refused at the interrupt, the
        // one place C can refuse it.
        send_array(handle.array_sender(), make_test_array(LAST + 1));
        gated.release();
        wait_until("the backlog drained", || {
            gated.processed().len() == LAST as usize
        });
        params_applied(&handle);

        assert_eq!(
            gated.processed(),
            (1..=LAST).collect::<Vec<_>>(),
            "everything already queued is processed; only new arrivals stop"
        );
        assert_eq!(
            port.read_int32_blocking(handle.ndarray_params.array_counter, 0)
                .unwrap(),
            LAST,
            "and each of them counts against ArrayCounter"
        );
        assert_eq!(
            port.read_int32_blocking(handle.plugin_params.dropped_arrays, 0)
                .unwrap(),
            0,
            "a frame refused at the interrupt is not a dropped array (C never \
             reaches driverCallback for it)"
        );
    }

    /// A plugin port answers the pool-control PVs, because
    /// `NDPluginDriver::writeInt32` hands every parameter below
    /// `FIRST_NDPLUGIN_PARAM` to `asynNDArrayDriver::writeInt32`
    /// (NDPluginDriver.cpp:690-692), whose pool branch is at
    /// asynNDArrayDriver.cpp:684-694. Leaving them unhandled froze
    /// `PoolAllocBuffers_RBV` and friends at their construction values for the
    /// plugin's whole life, so a client polling a plugin's pool read 0 no
    /// matter what the pool was doing.
    ///
    /// A plugin's own pool is built for the plugin, `new NDArrayPool(this,
    /// maxMemory)` (asynNDArrayDriver.cpp:881), so a refusal reports on the
    /// plugin's port (NDArrayPool.cpp:230-233).
    #[test]
    fn a_plugin_pool_refusal_reports_on_the_plugin_port() {
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("POOLREFUSE").unwrap(),
            PassthroughProcessor,
            64,
            4,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let log = std::env::temp_dir().join(format!("plugin-pool-{}.log", std::process::id()));
        let file = std::fs::File::create(&log).unwrap();
        handle.port_runtime().port_handle().trace().set_trace_file(
            Some("POOLREFUSE"),
            asyn_rs::trace::TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );

        let refused = handle
            .own_pool()
            .alloc(vec![NDDimension::new(100)], NDDataType::UInt8);
        assert!(refused.is_err());
        let text = std::fs::read_to_string(&log).unwrap();
        let _ = std::fs::remove_file(&log);
        assert!(
            text.contains("NDArrayPool::alloc:: error: reached limit of 64 memory (1 buffers)"),
            "{text}"
        );
    }

    /// A processor whose output comes from no pool, as a file plugin's read
    /// does.
    struct PoolLess(usize);
    impl NDPluginProcess for PoolLess {
        const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
        const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
        fn process_array(&self, _array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
            ProcessResult::arrays(vec![Arc::new(NDArray::new(
                vec![NDDimension::new(self.0)],
                NDDataType::UInt8,
            ))])
        }
        fn plugin_type(&self) -> &str {
            "PoolLess"
        }
    }

    /// C `endProcessCallbacks` copies an output into `pNDArrayPool`
    /// (NDPluginDriver.cpp:285-287), the input's pool since `:419`, so it is
    /// queued on the detector's driver.
    #[test]
    fn a_pool_less_output_is_copied_into_the_inputs_pool() {
        let wiring = test_wiring();
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("POOLLESS").unwrap(),
            PoolLess(4),
            1_000_000,
            4,
            0,
            "",
            wiring.clone(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);
        let (down, mut rx) = ndarray_channel("POOLLESS_DOWN", 4);
        wiring.subscribe(&down, "POOLLESS", 0).unwrap();

        let detector = DriverPool::standalone(1_000_000);
        let frame = detector
            .alloc(vec![NDDimension::new(4)], NDDataType::UInt8)
            .unwrap();
        send_array(handle.array_sender(), frame.share());
        let out = rx.blocking_recv().unwrap();
        assert_eq!(out.pool_id(), detector.id());
        let queued = detector.owner().queued().clone();
        wait_until("the detector's queue to drain", || queued.get() == 0);
    }

    /// A refused copy ends the frame with C's line on the plugin's port
    /// (NDPluginDriver.cpp:295-299); the pool's own line goes to the
    /// detector's.
    #[test]
    fn a_refused_output_copy_ends_processing_with_cs_line() {
        let wiring = test_wiring();
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("COPYREFUSE").unwrap(),
            PoolLess(100),
            1_000_000,
            4,
            0,
            "",
            wiring.clone(),
            DEFAULT_PLUGIN_THREADS,
        );
        let log = std::env::temp_dir().join(format!("copy-refuse-{}.log", std::process::id()));
        let file = std::fs::File::create(&log).unwrap();
        handle.port_runtime().port_handle().trace().set_trace_file(
            Some("COPYREFUSE"),
            asyn_rs::trace::TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );
        enable_callbacks(&handle);
        let (down, rx) = ndarray_channel("COPYREFUSE_DOWN", 4);
        wiring.subscribe(&down, "COPYREFUSE", 0).unwrap();

        let detector = DriverPool::standalone(64);
        let frame = detector
            .alloc(vec![NDDimension::new(4)], NDDataType::UInt8)
            .unwrap();
        send_array(handle.array_sender(), frame.share());
        let end = "NDPluginDriver::endProcessCallbacks: Couldn't allocate output array. \
                   Further processing terminated.";
        wait_until("the endProcessCallbacks line", || {
            std::fs::read_to_string(&log).is_ok_and(|t| t.contains(end))
        });
        let _ = std::fs::remove_file(&log);
        // Delivered, it would sit queued on the detector until received; the
        // input is released only after the line is printed.
        let queued = detector.owner().queued().clone();
        wait_until("the detector's queue to drain", || queued.get() == 0);
        drop(rx);
    }

    /// A pool-less output whose size the test sets between frames.
    struct Resized(Arc<std::sync::atomic::AtomicUsize>);
    impl NDPluginProcess for Resized {
        const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
        const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
        fn process_array(&self, _array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
            ProcessResult::arrays(vec![Arc::new(NDArray::new(
                vec![NDDimension::new(self.0.load(Ordering::Acquire))],
                NDDataType::UInt8,
            ))])
        }
        fn plugin_type(&self) -> &str {
            "Resized"
        }
    }

    /// C returns at a refused copy before `pArrays[0] = pArrayOut`
    /// (NDPluginDriver.cpp:288-299), so PoolPreAllocBuffers still sizes its
    /// buffers from the last copy that succeeded.
    #[test]
    fn a_refused_output_copy_keeps_the_previous_pre_alloc_template() {
        let size = Arc::new(std::sync::atomic::AtomicUsize::new(8));
        let wiring = test_wiring();
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("COPYKEEP").unwrap(),
            Resized(size.clone()),
            1_000_000,
            4,
            0,
            "",
            wiring.clone(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);
        let (down, mut rx) = ndarray_channel("COPYKEEP_DOWN", 4);
        wiring.subscribe(&down, "COPYKEEP", 0).unwrap();
        let port = handle.port_runtime().port_handle();
        let p = handle.ndarray_params;
        let frames = |n| {
            wait_until("the frame", || {
                port.read_int32_blocking(p.array_counter, 0).unwrap() == n
            })
        };

        let detector = DriverPool::standalone(1_000);
        let input = || {
            detector
                .alloc(vec![NDDimension::new(4)], NDDataType::UInt8)
                .unwrap()
                .share()
        };
        send_array(handle.array_sender(), input());
        drop(rx.blocking_recv().unwrap());
        frames(1);
        size.store(2_000, Ordering::Release);
        send_array(handle.array_sender(), input());
        frames(2);

        port.write_int32_blocking(p.pool_empty_free_list, 0, 1)
            .unwrap();
        port.write_int32_blocking(p.pool_num_pre_alloc_buffers, 0, 1)
            .unwrap();
        port.write_int32_blocking(p.pool_pre_alloc, 0, 1).unwrap();
        port.write_int32_blocking(p.pool_poll_stats, 0, 1).unwrap();
        params_applied(&handle);
        assert_eq!(
            port.read_int32_blocking(p.pool_free_buffers, 0).unwrap(),
            1,
            "pre-allocated from the 8-byte copy"
        );
    }

    /// A refused output, then one that would fit.
    struct RefusedThenSmall;
    impl NDPluginProcess for RefusedThenSmall {
        const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
        const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
        fn process_array(&self, _array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
            let out = |n| Arc::new(NDArray::new(vec![NDDimension::new(n)], NDDataType::UInt8));
            ProcessResult::arrays(vec![out(100), out(4)])
        }
        fn plugin_type(&self) -> &str {
            "RefusedThenSmall"
        }
    }

    /// "Further processing terminated" (NDPluginDriver.cpp:295-299): nothing
    /// after a refused copy goes downstream.
    #[test]
    fn nothing_after_a_refused_output_copy_is_delivered() {
        let wiring = test_wiring();
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("COPYSTOP").unwrap(),
            RefusedThenSmall,
            1_000_000,
            4,
            0,
            "",
            wiring.clone(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);
        let (down, rx) = ndarray_channel("COPYSTOP_DOWN", 4);
        wiring.subscribe(&down, "COPYSTOP", 0).unwrap();
        let port = handle.port_runtime().port_handle();
        let counter = handle.ndarray_params.array_counter;

        let detector = DriverPool::standalone(64);
        let frame = detector
            .alloc(vec![NDDimension::new(4)], NDDataType::UInt8)
            .unwrap();
        send_array(handle.array_sender(), frame.share());
        wait_until("the frame", || {
            port.read_int32_blocking(counter, 0).unwrap() == 1
        });
        assert_eq!(rx.pending(), 0);
    }

    /// An addressed post the pool cannot take.
    struct OversizedPost;
    impl NDPluginProcess for OversizedPost {
        const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
        const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
        fn process_array(&self, _array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
            let mut result = ProcessResult::sink(vec![]);
            result.addressed = vec![(
                0,
                Arc::new(NDArray::new(vec![NDDimension::new(100)], NDDataType::UInt8)),
            )];
            result
        }
        fn plugin_type(&self) -> &str {
            "OversizedPost"
        }
    }

    /// C's addressed posts allocate straight from `pNDArrayPool`
    /// (NDPluginTimeSeries.cpp:374); only `endProcessCallbacks` prints its
    /// own line at a refusal (NDPluginDriver.cpp:295-299).
    #[test]
    fn a_refused_addressed_post_prints_no_end_process_callbacks_line() {
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("POSTREFUSE").unwrap(),
            OversizedPost,
            1_000_000,
            4,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        // The post allocates from the frame's pool (NDPluginDriver.cpp:419),
        // so the frame comes from a pool whose port prints its refusal.
        let (source, _source_jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("POSTREFUSE_SRC").unwrap(),
            PassthroughProcessor,
            64,
            4,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let log = std::env::temp_dir().join(format!("post-refuse-{}.log", std::process::id()));
        let file = Arc::new(std::sync::Mutex::new(std::fs::File::create(&log).unwrap()));
        for (h, name) in [(&handle, "POSTREFUSE"), (&source, "POSTREFUSE_SRC")] {
            h.port_runtime()
                .port_handle()
                .trace()
                .set_trace_file(Some(name), asyn_rs::trace::TraceFile::File(file.clone()));
        }
        enable_callbacks(&handle);
        let port = handle.port_runtime().port_handle();
        let counter = handle.ndarray_params.array_counter;

        let frame = source
            .own_pool()
            .alloc(vec![NDDimension::new(4)], NDDataType::UInt8)
            .unwrap();
        send_array(handle.array_sender(), frame.share());
        wait_until("the frame", || {
            port.read_int32_blocking(counter, 0).unwrap() == 1
        });
        let text = std::fs::read_to_string(&log).unwrap();
        let _ = std::fs::remove_file(&log);
        assert!(
            text.contains("NDArrayPool::alloc:: error: reached limit of 64 memory (2 buffers)"),
            "{text}"
        );
        assert!(!text.contains("endProcessCallbacks"), "{text}");
    }

    /// One case per branch of that C `if`, not one per story.
    #[test]
    fn a_plugin_port_answers_the_pool_control_writes() {
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("POOLCTL").unwrap(),
            PassthroughProcessor,
            1_000_000,
            4,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let pool = handle.own_pool().clone();
        let port = handle.port_runtime().port_handle();
        let p = handle.ndarray_params;
        let alloc = || port.read_int32_blocking(p.pool_alloc_buffers, 0).unwrap();
        let free = || port.read_int32_blocking(p.pool_free_buffers, 0).unwrap();

        // PoolPollStats: the readbacks follow the pool.
        let held: Vec<_> = (0..3)
            .map(|_| {
                pool.alloc(vec![NDDimension::new(64)], NDDataType::UInt8)
                    .unwrap()
            })
            .collect();
        port.write_int32_blocking(p.pool_poll_stats, 0, 1).unwrap();
        params_applied(&handle);
        assert_eq!((alloc(), free()), (3, 0), "three buffers out, none free");

        for arr in held {
            pool.release(arr);
        }
        port.write_int32_blocking(p.pool_poll_stats, 0, 1).unwrap();
        params_applied(&handle);
        assert_eq!((alloc(), free()), (3, 3), "returned buffers are free");

        // PoolEmptyFreeList: drops them; the readbacks wait for a poll
        // (asynNDArrayDriver.cpp:684-685).
        port.write_int32_blocking(p.pool_empty_free_list, 0, 1)
            .unwrap();
        assert_eq!((alloc(), free()), (3, 3), "not republished");
        port.write_int32_blocking(p.pool_poll_stats, 0, 1).unwrap();
        assert_eq!((alloc(), free()), (0, 0), "the free list is gone");

        // PoolPreAllocBuffers: needs an array to take dimensions from, so run
        // one frame through first (C refuses outright without `pArrays[0]`,
        // asynNDArrayDriver.cpp:715-721).
        enable_callbacks(&handle);
        send_array(handle.array_sender(), make_test_array(1));
        params_applied(&handle);
        port.write_int32_blocking(p.pool_num_pre_alloc_buffers, 0, 2)
            .unwrap();
        port.write_int32_blocking(p.pool_pre_alloc, 0, 1).unwrap();
        params_applied(&handle);
        // Posted per copy, before the release (asynNDArrayDriver.cpp:731-737).
        port.write_int32_blocking(p.pool_poll_stats, 0, 1).unwrap();
        assert_eq!(free(), 2, "two buffers pre-allocated onto the free list");
        assert_eq!(
            port.read_int32_blocking(p.pool_pre_alloc, 0).unwrap(),
            0,
            "the command PV clears itself once it has run"
        );
    }

    #[test]
    fn test_downstream_receives_multiple() {
        let (ds1, mut rx1) = ndarray_channel("DS1", 10);
        let (ds2, mut rx2) = ndarray_channel("DS2", 10);
        let mut output = NDArrayOutput::new();
        output.add(ds1);
        output.add(ds2);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("DS_TEST").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);

        send_array(handle.array_sender(), make_test_array(77));

        // Both downstream receivers should have the array
        let r1 = rx1.blocking_recv().unwrap();
        let r2 = rx2.blocking_recv().unwrap();
        assert_eq!(r1.unique_id, 77);
        assert_eq!(r2.unique_id, 77);
    }

    #[test]
    fn test_param_updates_after_send() {
        struct ParamTracker;
        impl NDPluginProcess for ParamTracker {
            const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
            const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
            const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
            fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
                ProcessResult::arrays(vec![Arc::clone(array)])
            }
            fn plugin_type(&self) -> &str {
                "ParamTracker"
            }
        }

        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("PARAM_TEST").unwrap(),
            ParamTracker,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);

        // Send array
        send_array(handle.array_sender(), make_test_array(1));
        let received = downstream_rx.blocking_recv().unwrap();
        assert_eq!(received.unique_id, 1);

        // Write enable_callbacks — should not crash
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.enable_callbacks, 0, 1)
            .unwrap();
        params_applied(&handle);

        // Still works after param update
        send_array(handle.array_sender(), make_test_array(2));
        let received = downstream_rx.blocking_recv().unwrap();
        assert_eq!(received.unique_id, 2);
    }

    #[test]
    fn a_param_write_does_not_define_execution_time() {
        struct Echo;
        impl NDPluginProcess for Echo {
            const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
            const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
            const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
            fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
                ProcessResult::arrays(vec![Arc::clone(array)])
            }
            fn on_param_change(
                &self,
                reason: usize,
                params: &PluginParamSnapshot,
            ) -> ParamChangeResult {
                ParamChangeResult::updates(vec![ParamUpdate::int32(reason, params.value.as_i32())])
            }
            fn plugin_type(&self) -> &str {
                "Echo"
            }
        }

        let (handle, _jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("EXEC_TIME").unwrap(),
            Echo,
            1_000_000,
            10,
            0,
            NDArrayOutput::new(),
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        enable_callbacks(&handle);
        params_applied(&handle);
        assert!(
            port.read_float64_blocking(handle.plugin_params.execution_time, 0)
                .is_err()
        );

        send_array(handle.array_sender(), make_test_array(1));
        params_applied(&handle);
        assert!(
            port.read_float64_blocking(handle.plugin_params.execution_time, 0)
                .is_ok()
        );
    }

    #[test]
    fn test_sort_buffer_reorders_by_unique_id() {
        let mut buf = SortBuffer::new();

        // Insert out of order: 3, 1, 2
        buf.insert(3, vec![make_test_array(3)], 10);
        buf.insert(1, vec![make_test_array(1)], 10);
        buf.insert(2, vec![make_test_array(2)], 10);

        assert_eq!(buf.len(), 3);

        let drained = buf.drain_all();
        let ids: Vec<i32> = drained.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![1, 2, 3], "should drain in sorted uniqueId order");
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.prev_unique_id, 3);
    }

    #[test]
    fn test_sort_buffer_drain_ready_contiguous() {
        // B3: drain_ready releases the head while the next-expected uniqueId
        // is contiguous, even when later ids are still missing.
        let mut buf = SortBuffer::new();
        // Mark a prior emission (prev=0) so the contiguity path is active;
        // C++ only uses the deadline for the very first output array.
        buf.note_emitted(0);
        buf.insert(1, vec![make_test_array(1)], 10);
        buf.insert(2, vec![make_test_array(2)], 10);
        buf.insert(5, vec![make_test_array(5)], 10); // gap: 3,4 missing

        // sort_time large → only contiguity drives release.
        let drained = buf.drain_ready(100.0, std::time::Instant::now());
        let ids: Vec<i32> = drained.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![1, 2], "contiguous run released; id=5 held by gap");
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn test_sort_buffer_drain_ready_deadline() {
        // B3: a stale head is released past sort_time even with a gap.
        let mut buf = SortBuffer::new();
        buf.note_emitted(1); // prev=1
        buf.insert(5, vec![make_test_array(5)], 10); // out of order
        std::thread::sleep(std::time::Duration::from_millis(30));
        // sort_time=0.01s → head aged past deadline → released.
        let drained = buf.drain_ready(0.01, std::time::Instant::now());
        let ids: Vec<i32> = drained.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![5], "stale head released via deadline");
    }

    /// C sortingTask releases a head only once `deltaTime > sortTime`
    /// (NDPluginDriver.cpp:662): held at and before the deadline, released
    /// the instant after it, whatever fraction of it has passed.
    #[test]
    fn sort_buffer_releases_a_head_only_past_sort_time() {
        const SORT_TIME: f64 = 1.0;
        let mut buf = SortBuffer::new();
        buf.note_emitted(1);
        buf.insert(5, vec![make_test_array(5)], 10);
        let inserted = buf.entries[&5].inserted;
        let deadline = std::time::Duration::from_secs_f64(SORT_TIME);
        for held in [deadline / 2, deadline - deadline / 100, deadline] {
            assert!(
                buf.drain_ready(SORT_TIME, inserted + held).is_empty(),
                "held at {held:?} of {deadline:?}"
            );
        }
        let past = inserted + deadline + std::time::Duration::from_nanos(1);
        let ids: Vec<i32> = buf
            .drain_ready(SORT_TIME, past)
            .iter()
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(ids, vec![5], "released once past the deadline");
    }

    #[test]
    fn test_sort_buffer_detects_disordered_on_emit() {
        // B4: disorder is counted at emission time.
        let mut buf = SortBuffer::new();
        buf.note_emitted(5); // prev=5, first_output now false
        buf.note_emitted(3); // 3 != 5 and != 6 → disordered
        assert_eq!(buf.disordered_arrays, 1);
        buf.note_emitted(4); // 4 != 3 and != 4? 4 == prev+1 → ordered
        assert_eq!(buf.disordered_arrays, 1);
    }

    #[test]
    fn test_sort_buffer_drops_when_full() {
        let mut buf = SortBuffer::new();

        // sort_size=2: third insert is refused.
        assert!(buf.insert(1, vec![make_test_array(1)], 2));
        assert!(buf.insert(2, vec![make_test_array(2)], 2));
        assert!(!buf.insert(3, vec![make_test_array(3)], 2));

        assert_eq!(buf.len(), 2);
        assert_eq!(buf.dropped_output_arrays, 1);
    }

    #[test]
    fn test_constructor_initialises_c_read_only_params() {
        // C++ NDPluginDriver.cpp:152-160 initialises these in the constructor
        // precisely so a read before any array flows returns a value instead
        // of "uninitialized". Nothing here enables callbacks or sends an
        // array: this is the idle plugin an operator sees right after
        // iocInit, which is when QueueFree_RBV read 0 (full) and
        // MaxThreads_RBV read UDF/INVALID.
        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("CTOR_TEST").unwrap(),
            PassthroughProcessor,
            1_000_000,
            20,
            0,
            NDArrayOutput::new(),
            ("CTOR_UP", 3),
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        let read = |reason: usize| port.read_int32_blocking(reason, 0);

        // C `:156` / `:157` — an idle queue is entirely free.
        assert_eq!(read(handle.plugin_params.queue_size).unwrap(), 20);
        assert_eq!(read(handle.plugin_params.queue_use).unwrap(), 20);
        // C `:153` / `:158` / `:159`. These have no other writer: MaxThreads
        // is a longin with SCAN "I/O Intr", no PINI and no output partner, so
        // an unset param leaves it UDF forever.
        assert_eq!(read(handle.plugin_params.nd_array_addr).unwrap(), 3);
        assert_eq!(
            read(handle.plugin_params.max_threads).unwrap(),
            PLUGIN_MAX_THREADS
        );
        assert_eq!(
            read(handle.plugin_params.num_threads).unwrap(),
            PLUGIN_NUM_THREADS
        );
        // C `:154` / `:155`.
        assert_eq!(read(handle.plugin_params.dropped_arrays).unwrap(), 0);
        assert_eq!(read(handle.plugin_params.dropped_output_arrays).unwrap(), 0);
    }

    /// C hands `maxThreads` to the `NDPluginDriver` constructor and floors it
    /// there (NDPluginDriver.cpp:117, :158): the port starts holding it, and
    /// no `writeInt32` override ever sees it written.
    #[test]
    fn max_threads_is_a_constructor_argument() {
        struct Recorder(Arc<parking_lot::Mutex<Vec<usize>>>);
        impl NDPluginProcess for Recorder {
            const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
            const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
            const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
            fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
                ProcessResult::forward(array, vec![])
            }
            fn plugin_type(&self) -> &str {
                "Recorder"
            }
            fn validate_int32(
                &self,
                reason: usize,
                _addr: i32,
                _value: i32,
                verdict: WriteVerdict<i32>,
                _params: &mut WritePort<'_>,
            ) -> WriteOutcome<i32> {
                self.0.lock().push(reason);
                verdict.into()
            }
        }
        for (port, given, held) in [("MAXTHR_FOUR", 4, 4), ("MAXTHR_ZERO", 0, 1)] {
            let seen = Arc::default();
            let (handle, _data_jh) = create_plugin_runtime(
                &asyn_rs::asyn_record::claim_port(port).unwrap(),
                Recorder(Arc::clone(&seen)),
                1_000_000,
                4,
                0,
                ("MAXTHR_UP", 0),
                test_wiring(),
                DEFAULT_PLUGIN_THREADS.with_max_threads(given),
            );
            let max_threads = handle.plugin_params.max_threads;
            let port_handle = handle.port_runtime().port_handle();
            assert_eq!(
                port_handle.read_int32_blocking(max_threads, 0).unwrap(),
                held
            );
            assert!(!seen.lock().contains(&max_threads), "{port}");
        }
    }

    /// Observable half of `OverlapProcessor`, kept in the test so the
    /// processor itself can be moved into the runtime.
    #[derive(Clone, Default)]
    struct OverlapProbe {
        inside: Arc<parking_lot::Mutex<usize>>,
        gate: Arc<parking_lot::Condvar>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }

    /// Records the peak number of concurrent `process_array` calls by holding
    /// every frame until `want` of them are inside at once. A plugin that
    /// processes frames one at a time can never drive the peak above 1,
    /// whatever the timing.
    struct OverlapProcessor {
        probe: OverlapProbe,
        want: usize,
        budget: std::time::Duration,
    }

    impl NDPluginProcess for OverlapProcessor {
        const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
        const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
        fn process_array(&self, _array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
            let mut inside = self.probe.inside.lock();
            *inside += 1;
            let peak = self
                .probe
                .peak
                .fetch_max(*inside, Ordering::AcqRel)
                .max(*inside);
            if peak >= self.want {
                self.probe.gate.notify_all();
            } else {
                // Wait on the *peak*, not on the live count: it only ever
                // rises, so a frame that returns before its neighbours wake
                // cannot re-park them.
                let deadline = std::time::Instant::now() + self.budget;
                while self.probe.peak.load(Ordering::Acquire) < self.want
                    && std::time::Instant::now() < deadline
                {
                    self.probe.gate.wait_until(&mut inside, deadline);
                }
            }
            *inside -= 1;
            ProcessResult::empty()
        }

        fn plugin_type(&self) -> &str {
            "Overlap"
        }
    }

    #[test]
    fn test_num_threads_write_spawns_overlapping_callback_threads() {
        // C runs `NumThreads` callback threads over one input queue
        // (NDPluginDriver.cpp:996-1001) and tears them down and respawns them
        // when NumThreads is written (`:730-733`). Four frames that each hold
        // until four are inside can only all return if four threads really
        // are running — which is what the NumThreads PV exists to buy.
        let probe = OverlapProbe::default();
        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("NTHREADS_TEST").unwrap(),
            OverlapProcessor {
                probe: probe.clone(),
                want: 4,
                budget: std::time::Duration::from_secs(2),
            },
            1_000_000,
            10,
            0,
            NDArrayOutput::new(),
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        // NumThreads is clamped to [1, MaxThreads] (C `createCallbackThreads`
        // NDPluginDriver.cpp:955-971), so MaxThreads has to move first.
        port.write_int32_blocking(handle.plugin_params.max_threads, 0, 4)
            .unwrap();
        params_applied(&handle);
        port.write_int32_blocking(handle.plugin_params.num_threads, 0, 4)
            .unwrap();
        params_applied(&handle);
        assert_eq!(
            port.read_int32_blocking(handle.plugin_params.num_threads, 0)
                .unwrap(),
            4,
            "NumThreads_RBV must report the accepted value"
        );

        enable_callbacks(&handle);
        for id in 1..=4 {
            send_array(handle.array_sender(), make_test_array(id));
        }
        // Quiescence: the queue is empty AND every dispatched frame is back.
        params_applied(&handle);

        assert_eq!(
            probe.peak.load(Ordering::Acquire),
            4,
            "four callback threads must process four frames concurrently"
        );
    }

    /// Records the stack of the thread each hook runs on: `on_param_change`
    /// on the data thread, `process_array` on a callback thread.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[derive(Clone, Default)]
    struct StackProbe {
        data: Arc<parking_lot::Mutex<Vec<usize>>>,
        callback: Arc<parking_lot::Mutex<Vec<usize>>>,
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    impl NDPluginProcess for StackProbe {
        const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
        const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
        fn process_array(&self, _array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
            let bytes = epics_libcom_rs::runtime::task::current_thread_stack_bytes();
            self.callback.lock().push(bytes.expect("stack readback"));
            ProcessResult::empty()
        }
        fn on_param_change(
            &self,
            _reason: usize,
            _params: &PluginParamSnapshot,
        ) -> ParamChangeResult {
            let bytes = epics_libcom_rs::runtime::task::current_thread_stack_bytes();
            self.data.lock().push(bytes.expect("stack readback"));
            ParamChangeResult::empty()
        }
        fn plugin_type(&self) -> &str {
            "StackProbe"
        }
    }

    /// A configured `stackSize` is the stack of the data thread and of every
    /// callback thread, including the ones a NumThreads write respawns: C
    /// hands `threadStackSize_` to all of them (asynNDArrayDriver.cpp:876-877,
    /// NDPluginDriver.cpp:1000).
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn a_configured_stack_size_is_the_stack_of_every_plugin_thread() {
        // glibc hands a new thread the cached stack of an exited one when it
        // is up to 4x the request, and the readback reports that block, so
        // each value runs in a child of its own, where every exited thread is
        // one of the plugin's.
        const CHILD: &str = "AD_CORE_STACK_PIN_RAW";
        let Some(raw) = std::env::var_os(CHILD) else {
            let name = "plugin::runtime::tests::a_configured_stack_size_is_the_stack_of_every_plugin_thread";
            for raw in ["0", "-1", "1", "2", "500000"] {
                let out = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", name, "--nocapture"])
                    .env(CHILD, raw)
                    .output()
                    .unwrap();
                let stdout = String::from_utf8_lossy(&out.stdout);
                assert!(
                    out.status.success() && stdout.contains("1 passed"),
                    "stackSize {raw}: {stdout}{}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            return;
        };
        let raw: i64 = raw.to_str().unwrap().parse().unwrap();
        let spec = match raw {
            500_000 => StackSpec::Bytes(500_000),
            2 => StackSpec::Class(StackSizeClass::Big),
            _ => StackSpec::Class(StackSizeClass::Medium),
        };
        assert_eq!(plugin_stack(raw), spec, "stackSize {raw}");
        // glibc reports 499968 for a 500000 request.
        let within = |got: usize, want: usize| got <= want && want - got <= 64;
        // Frames go out from this thread: `send_array`'s helper thread would
        // leave a 2 MiB stack in the cache.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let publish = |handle: &PluginRuntimeHandle, id| {
            rt.block_on(handle.array_sender().publish(make_test_array(id)))
        };
        let probe = StackProbe::default();
        let (handle, _data_jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port(&format!("STACK_{raw}")).unwrap(),
            probe.clone(),
            1_000_000,
            10,
            0,
            "",
            test_wiring(),
            plugin_threads(0, raw),
        );
        assert_eq!(handle.threads().stack, spec);
        enable_callbacks(&handle);
        publish(&handle, 1);
        params_applied(&handle);

        let port = handle.port_runtime().port_handle();
        port.write_int32_blocking(handle.plugin_params.max_threads, 0, 2)
            .unwrap();
        port.write_int32_blocking(handle.plugin_params.num_threads, 0, 2)
            .unwrap();
        params_applied(&handle);
        publish(&handle, 2);
        params_applied(&handle);

        let want = spec.bytes();
        let data = probe.data.lock().clone();
        let callback = probe.callback.lock().clone();
        assert!(!data.is_empty(), "stackSize {raw}: no data-thread hook ran");
        assert_eq!(callback.len(), 2, "stackSize {raw}: one frame per pool");
        for got in data.iter().chain(&callback) {
            assert!(
                within(*got, want),
                "stackSize {raw}: a plugin thread has {got} bytes of stack, want {want} \
                     (data {data:?}, callback {callback:?})"
            );
        }
    }

    /// A configured `priority` is the band of the data thread and of every
    /// callback thread, including the ones a NumThreads write respawns, and
    /// `priority <= 0` is Medium (asynNDArrayDriver.cpp:878,
    /// NDPluginDriver.cpp:1016). Run with RT off: under RT, 200 maps past the
    /// FIFO range and the data thread is refused.
    #[test]
    fn a_configured_priority_is_the_band_of_every_plugin_thread() {
        use epics_libcom_rs::runtime::task::C_RT_PRIORITY_ENV;
        const CHILD: &str = "AD_CORE_PRIO_PIN_CHILD";
        const TEST: &str =
            "plugin::runtime::tests::a_configured_priority_is_the_band_of_every_plugin_thread";
        if std::env::var_os(CHILD).is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD, "1")
                .env(C_RT_PRIORITY_ENV, "NO")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success() && stdout.contains("1 passed"),
                "{stdout}{}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        // A thread registers in its own prologue, after the spawn returns.
        let osipri = |name: &str| -> Vec<u32> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let rows: Vec<u32> = epics_libcom_rs::runtime::task::thread_report()
                    .iter()
                    .filter(|t| t.name() == name)
                    .map(|t| t.epics_priority())
                    .collect();
                if !rows.is_empty() || std::time::Instant::now() > deadline {
                    return rows;
                }
                std::thread::yield_now();
            }
        };
        let cases = [(0, 50), (-1, 50), (1, 1), (30, 30), (200, 200)];
        for (with_output, (raw, want)) in [false, true]
            .into_iter()
            .flat_map(|o| cases.map(|c| (o, c)))
        {
            let port = format!("PRIO_{with_output}_{raw}");
            let (handle, _data_jh) = if with_output {
                create_plugin_runtime_with_output(
                    &asyn_rs::asyn_record::claim_port(&port).unwrap(),
                    PassthroughProcessor,
                    1_000_000,
                    10,
                    0,
                    NDArrayOutput::new(),
                    "",
                    test_wiring(),
                    plugin_threads(raw, 0),
                )
            } else {
                create_plugin_runtime(
                    &asyn_rs::asyn_record::claim_port(&port).unwrap(),
                    PassthroughProcessor,
                    1_000_000,
                    10,
                    0,
                    "",
                    test_wiring(),
                    plugin_threads(raw, 0),
                )
            };
            enable_callbacks(&handle);
            let p = handle.port_runtime().port_handle();
            p.write_int32_blocking(handle.plugin_params.max_threads, 0, 2)
                .unwrap();
            p.write_int32_blocking(handle.plugin_params.num_threads, 0, 2)
                .unwrap();
            params_applied(&handle);
            for name in [
                format!("plugin-data-{port}"),
                format!("{port}_Plugin_1"),
                format!("{port}_Plugin_2"),
            ] {
                // A respawned pool's old thread may not have deregistered yet.
                let rows = osipri(&name);
                assert!(
                    !rows.is_empty() && rows.iter().all(|p| *p == want),
                    "priority {raw}: {name} has {rows:?}, want {want}"
                );
            }
            // The port's own thread takes the raw value, as C's
            // `registerPort` does for a can-block port (asynManager.c:2077).
            let port_want = if raw == 0 { 50 } else { raw as u32 };
            let rows = osipri(&port);
            assert_eq!(rows, [port_want], "priority {raw}: the port thread");
        }
    }

    /// `registerPort` gets the `*Configure` values before
    /// `asynNDArrayDriver.cpp:876-879` adjusts them, as `unsigned int`
    /// (asynPortDriver.cpp:4031-4036).
    #[test]
    fn the_port_registers_with_the_raw_configure_values() {
        for (priority, stack, want_priority, want_stack) in [
            (0, 0, 0, 0),
            (-1, -1, u32::MAX, u32::MAX as usize),
            (30, 500_000, 30, 500_000),
        ] {
            let config = plugin_threads(priority, stack).port_config();
            assert_eq!(
                (config.priority, config.stack_size),
                (want_priority, want_stack),
                "Configure priority {priority}, stackSize {stack}"
            );
        }
        let config = DEFAULT_PLUGIN_THREADS.port_config();
        assert_eq!((config.priority, config.stack_size), (0, 0));
    }

    #[test]
    fn test_default_num_threads_processes_one_frame_at_a_time() {
        // C's constructor starts NumThreads at 1 (NDPluginDriver.cpp:159), so
        // an untouched plugin must still serialise its frames.
        let probe = OverlapProbe::default();
        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("NTHREADS1_TEST").unwrap(),
            OverlapProcessor {
                probe: probe.clone(),
                want: 2,
                budget: std::time::Duration::from_millis(300),
            },
            1_000_000,
            10,
            0,
            NDArrayOutput::new(),
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);
        for id in 1..=2 {
            send_array(handle.array_sender(), make_test_array(id));
        }
        params_applied(&handle);

        assert_eq!(
            probe.peak.load(Ordering::Acquire),
            1,
            "one callback thread must not overlap frames"
        );
    }

    #[test]
    fn test_constructor_initialises_the_ndarray_read_only_block() {
        // C++ `NDPluginDriver` derives from `asynNDArrayDriver`, so the base
        // constructor's block (asynNDArrayDriver.cpp:954-1005) runs for a
        // plugin as well. Read through the port handle, which is the same
        // path a record takes: an unwritten param comes back as an error.
        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("NDCTOR_TEST").unwrap(),
            PassthroughProcessor,
            2_097_152,
            20,
            0,
            NDArrayOutput::new(),
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        let p = &handle.ndarray_params;
        for (name, reason, want) in [
            ("ARRAY_SIZE_X", p.array_size_x, 0),
            ("ARRAY_SIZE_Y", p.array_size_y, 0),
            ("ARRAY_SIZE_Z", p.array_size_z, 0),
            ("ARRAY_SIZE", p.array_size, 0),
            ("ND_DIMENSIONS", p.n_dimensions, 0),
            (
                "COLOR_MODE",
                p.color_mode,
                crate::color::NDColorMode::Mono as i32,
            ),
            ("UNIQUE_ID", p.unique_id, 0),
            ("EPICS_TS_SEC", p.epics_ts_sec, 0),
            ("EPICS_TS_NSEC", p.epics_ts_nsec, 0),
            ("BAYER_PATTERN", p.bayer_pattern, 0),
            ("ARRAY_COUNTER", p.array_counter, 0),
            ("NUM_CAPTURED", p.num_captured, 0),
            ("FREE_CAPTURE", p.free_capture, 0),
            (
                "ND_ATTRIBUTES_STATUS",
                p.attributes_status,
                crate::driver::ndarray_driver::ATTR_STATUS_FILE_NOT_FOUND,
            ),
            ("NUM_QUEUED_ARRAYS", p.num_queued_arrays, 0),
            ("POOL_ALLOC_BUFFERS", p.pool_alloc_buffers, 0),
            ("POOL_FREE_BUFFERS", p.pool_free_buffers, 0),
        ] {
            assert_eq!(
                port.read_int32_blocking(reason, 0)
                    .unwrap_or_else(|e| panic!("{name} unset after construction: {e:?}")),
                want,
                "{name}"
            );
        }
        assert_eq!(
            port.read_float64_blocking(p.pool_max_memory, 0)
                .expect("POOL_MAX_MEMORY unset after construction"),
            0.0
        );
        assert_eq!(
            port.read_float64_blocking(p.pool_used_memory, 0)
                .expect("POOL_USED_MEMORY unset after construction"),
            0.0
        );
        assert_eq!(
            port.read_float64_blocking(p.timestamp_rbv, 0)
                .expect("TIME_STAMP unset after construction"),
            0.0
        );
        // Only NDPluginFile writes NDFullFileName (NDPluginFile.cpp:52, :157).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(
            rt.block_on(port.read_octet(p.full_file_name, 0, 256))
                .status()
                .is_err()
        );
        // NDPluginDriver.cpp:152-160 leaves EnableCallbacks to the template,
        // whose `$(ENABLED)` a defined value would override at init.
        assert!(
            port.read_int32_blocking(handle.plugin_params.enable_callbacks, 0)
                .is_err()
        );
        // An unseeded plugin leaves NDArrayCallbacks to the template too.
        assert!(port.read_int32_blocking(p.array_callbacks, 0).is_err());
    }

    /// A full sort buffer refuses arrays from the very first frame, because
    /// C judges the first output array by the same `orderOK` rule as every
    /// later one: `prevUniqueId_` starts at -1000 (NDPluginDriver.cpp:99), so
    /// nothing a detector ever sends is adjacent to it.
    ///
    /// Waving the first array past the buffer instead makes every later one
    /// in-order behind it, and the buffer that C fills and overflows here
    /// stays empty for the plugin's whole life.
    #[test]
    fn a_full_sort_buffer_drops_output_arrays_from_the_first_frame() {
        const SORT_SIZE: i32 = 3;
        const SENT: i32 = 8;
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 32);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);
        let (handle, _jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("SORT_FULL").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        enable_callbacks(&handle);
        port.write_int32_blocking(handle.plugin_params.sort_size, 0, SORT_SIZE)
            .unwrap();
        // Long enough that the sort deadline cannot fire during the test, so
        // what is measured is the admission rule and not a race with it.
        port.write_float64_blocking(handle.plugin_params.sort_time, 0, 30.0)
            .unwrap();
        port.write_int32_blocking(handle.plugin_params.sort_mode, 0, 1)
            .unwrap();
        params_applied(&handle);

        for id in 1..=SENT {
            send_array(handle.array_sender(), make_test_array(id));
        }
        params_applied(&handle);

        let mut emitted = Vec::new();
        while let Some(m) = downstream_rx.try_recv_msg() {
            emitted.push(m.array.unique_id);
        }
        assert!(
            emitted.is_empty(),
            "nothing may leave before the sort deadline, got {emitted:?}"
        );
        assert_eq!(
            port.read_int32_blocking(handle.plugin_params.sort_free, 0)
                .unwrap(),
            0,
            "the buffer holds SORT_SIZE arrays"
        );
        assert_eq!(
            port.read_int32_blocking(handle.plugin_params.dropped_output_arrays, 0)
                .unwrap(),
            SENT - SORT_SIZE,
            "every array past the buffer's capacity is a dropped output array"
        );
    }

    #[test]
    fn test_sort_mode_runtime_integration() {
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("SORT_TEST").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);

        // Enable sort mode with sort_size=10 and a sort_time deadline.
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.sort_size, 0, 10)
            .unwrap();
        handle
            .port_runtime()
            .port_handle()
            .write_float64_blocking(handle.plugin_params.sort_time, 0, 0.1)
            .unwrap();
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.sort_mode, 0, 1)
            .unwrap();
        params_applied(&handle);

        // Arrays 1,2,3 are all out of order against the -1000 sentinel, so
        // sorted mode buffers every one of them: nothing reaches the client
        // until the sort deadline releases the head (C sortingTask `:662`,
        // `deltaTime > sortTime`), and the rest then cascade out behind it
        // because each is adjacent to the one just emitted. The head is
        // stamped after `sent`, so its arrival can only come later than the
        // deadline measured from there, however late this thread runs.
        let sent = std::time::Instant::now();
        send_array(handle.array_sender(), make_test_array(1));
        send_array(handle.array_sender(), make_test_array(2));
        send_array(handle.array_sender(), make_test_array(3));

        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 1);
        assert!(
            sent.elapsed() >= std::time::Duration::from_millis(100),
            "the first array is held for the sort deadline, not emitted at once"
        );
        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 2);
        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 3);
        // C sortingTask only delivers (NDPluginDriver.cpp:635-690): the
        // drain's batch, SortFree back to full, leaves ArrayCounter as the
        // three beginProcessCallbacks set it.
        let port = handle.port_runtime().port_handle().clone();
        wait_until("the drain to post SortFree", || {
            port.read_int32_blocking(handle.plugin_params.sort_free, 0)
                .is_ok_and(|v| v == 10)
        });
        assert_eq!(
            port.read_int32_blocking(handle.ndarray_params.array_counter, 0)
                .unwrap(),
            3
        );

        // B3: now send out of order (5 before 4). prev=3, so 4 is in-order
        // and emitted immediately; 5 arrives first, is buffered, then 4
        // unblocks it. A deadline that can fire between the two sends would
        // release 5 alone and correctly put 4 behind it, so it is raised out
        // of reach and only contiguity can release 5.
        handle
            .port_runtime()
            .port_handle()
            .write_float64_blocking(handle.plugin_params.sort_time, 0, 30.0)
            .unwrap();
        params_applied(&handle);
        send_array(handle.array_sender(), make_test_array(5));
        send_array(handle.array_sender(), make_test_array(4));
        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 4);
        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 5);
    }

    #[test]
    fn sort_free_stays_undefined_until_sorted_mode() {
        let (handle, _jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("SORT_FREE_UDF").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            NDArrayOutput::new(),
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        enable_callbacks(&handle);
        port.write_int32_blocking(handle.plugin_params.sort_mode, 0, 0)
            .unwrap();
        send_array(handle.array_sender(), make_test_array(1));
        params_applied(&handle);
        assert!(
            port.read_int32_blocking(handle.plugin_params.sort_free, 0)
                .is_err(),
            "unsorted processing and a SortMode=0 write leave SortFree undefined"
        );

        port.write_int32_blocking(handle.plugin_params.sort_mode, 0, 1)
            .unwrap();
        send_array(handle.array_sender(), make_test_array(2));
        params_applied(&handle);
        assert_eq!(
            port.read_int32_blocking(handle.plugin_params.sort_free, 0)
                .unwrap(),
            10
        );
    }

    #[test]
    fn test_control_plane_write_resets_every_mirrored_counter() {
        // B12/B15 boundary test. C++ holds ArrayCounter, DroppedArrays,
        // DroppedOutputArrays and DisorderedArrays in the param library and
        // increments each with a read-modify-write, so a PV write resets the
        // count. Rust mirrors each in a working copy; if a write does not land
        // back in that copy, the next status publish silently reverts it.
        //
        // One case per counter, not one per scenario: the boundary is
        // "written value survives the next publish", and it is the same
        // boundary for all four regardless of how they came to be non-zero.
        let (handle, _data_jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("COUNTER_RESET_TEST").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);

        let port = handle.port_runtime().port_handle().clone();
        let counters: [(&str, usize); 4] = [
            ("ARRAY_COUNTER", handle.ndarray_params.array_counter),
            ("DROPPED_ARRAYS", handle.plugin_params.dropped_arrays),
            (
                "DROPPED_OUTPUT_ARRAYS",
                handle.plugin_params.dropped_output_arrays,
            ),
            ("DISORDERED_ARRAYS", handle.plugin_params.disordered_arrays),
        ];

        for (name, reason) in counters {
            // Write a distinct non-zero value through the control plane.
            port.write_int32_blocking(reason, 0, 7).unwrap();
            params_applied(&handle);

            // Drive one array so the data path publishes its status batch.
            // Before the fix the batch carried the working copy's stale 0 and
            // stomped the 7; ArrayCounter additionally increments, so it must
            // read 8 rather than restarting from 1.
            send_array(handle.array_sender(), make_test_array(1));
            params_applied(&handle);

            let want = if reason == handle.ndarray_params.array_counter {
                8
            } else {
                7
            };
            wait_until(&format!("{name} to keep its control-plane value"), || {
                port.read_int32_blocking(reason, 0).is_ok_and(|v| v == want)
            });
            assert_eq!(
                port.read_int32_blocking(reason, 0).unwrap(),
                want,
                "{name}: a control-plane write was reverted by the next publish"
            );
        }
    }

    #[test]
    fn test_throttle_drops_output_arrays() {
        // G7: with a tiny MaxByteRate, output arrays exceeding the byte budget
        // are dropped and counted into DroppedOutputArrays.
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("THROTTLE_TEST").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);

        // MaxByteRate = 8 bytes/sec. Each test array is 4 bytes; the bucket
        // starts full at 8, so the first two pass and the rest are dropped.
        handle
            .port_runtime()
            .port_handle()
            .write_float64_blocking(handle.plugin_params.max_byte_rate, 0, 8.0)
            .unwrap();
        params_applied(&handle);

        for id in 1..=5 {
            send_array(handle.array_sender(), make_test_array(id));
        }
        // ArrayCounter counts every processed frame (throttle drops happen on
        // the output side, after counting), and its flush follows the array
        // publish — so counter == 5 means everything that will ever reach the
        // downstream queue is already there.
        let port = handle.port_runtime().port_handle().clone();
        let counter = handle.ndarray_params.array_counter;
        wait_until("all 5 frames to be processed", || {
            port.read_int32_blocking(counter, 0).is_ok_and(|v| v == 5)
        });

        // Drain whatever made it through — strictly fewer than 5.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut received = 0;
        while rt
            .block_on(async {
                tokio::time::timeout(std::time::Duration::from_millis(20), downstream_rx.recv())
                    .await
            })
            .map(|o| o.is_some())
            .unwrap_or(false)
        {
            received += 1;
        }
        assert!(
            received < 5,
            "throttle must drop some arrays (got {received})"
        );
        assert!(received >= 1, "first array within budget must pass");
    }

    #[test]
    fn test_process_plugin_reprocesses_last_input() {
        // G5: writing ProcessPlugin re-injects the cached last input array.
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("PROCESS_PLUGIN_TEST").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);

        send_array(handle.array_sender(), make_test_array(7));
        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 7);

        // Trigger ProcessPlugin — the cached input (id=7) is reprocessed.
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.process_plugin, 0, 1)
            .unwrap();
        let reprocessed = downstream_rx.blocking_recv().unwrap();
        assert_eq!(
            reprocessed.unique_id, 7,
            "ProcessPlugin re-emits last input"
        );
    }

    #[test]
    fn test_process_plugin_zero_reprocesses_last_input() {
        // C NDPluginDriver.cpp:755 re-injects on any ProcessPlugin write, so
        // `caput ProcessPlugin 0` re-processes too.
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("PROCESS_PLUGIN_ZERO_TEST").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);

        send_array(handle.array_sender(), make_test_array(7));
        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 7);

        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.process_plugin, 0, 0)
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let reprocessed = loop {
            if let Some(msg) = downstream_rx.try_recv_msg() {
                break msg;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "ProcessPlugin 0 re-emits last input"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(reprocessed.array.array().unique_id, 7);
    }

    #[test]
    fn test_dimensions_read_serves_the_last_input_dims() {
        // C readInt32Array (NDPluginDriver.cpp:878-882) serves dimsPrev_, so
        // a .PROC of Dimensions_RBV reads the last frame's dims.
        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("DIMS_READ_TEST").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            NDArrayOutput::new(),
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);
        let array = NDArray::new(
            vec![NDDimension::new(3), NDDimension::new(5)],
            NDDataType::UInt8,
        );
        send_array(handle.array_sender(), published(Arc::new(array)));
        let port = handle.port_runtime().port_handle().clone();
        let counter = handle.ndarray_params.array_counter;
        wait_until("the frame to be counted", || {
            port.read_int32_blocking(counter, 0).is_ok_and(|v| v == 1)
        });
        let dims = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(port.read_int32_array(handle.ndarray_params.array_dimensions, 0, 10))
            .unwrap();
        assert_eq!(&dims[..], &[3, 5, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn test_min_callback_time_throttle_not_counted() {
        // A MinCallbackTime-throttled array is silently skipped, NOT counted.
        // C++ driverCallback (NDPluginDriver.cpp:405-450) falls through the
        // `deltaTime <= minCallbackTime` gate straight to callParamCallbacks()
        // without touching DroppedArrays — that counter is incremented ONLY on
        // a compression-unaware array (:388) or a full message queue (:440).
        // Verify (a) the throttled array is not emitted and (b) DroppedArrays
        // stays zero.
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("MIN_CB_TEST").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);
        let dropped = handle.array_sender().dropped_arrays_counter().clone();

        // 10s minimum between callbacks — only the first array gets through.
        handle
            .port_runtime()
            .port_handle()
            .write_float64_blocking(handle.plugin_params.min_callback_time, 0, 10.0)
            .unwrap();
        params_applied(&handle);

        send_array(handle.array_sender(), make_test_array(1));
        send_array(handle.array_sender(), make_test_array(2));

        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 1);
        // Fence the param channel: the barrier acks only once the array queue
        // has drained, so array 1's processing is finished before the negative
        // checks below. Array 2 was refused at the gate and never queued.
        params_applied(&handle);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let second = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_millis(50), downstream_rx.recv()).await
        });
        assert!(
            second.is_err(),
            "second array throttled out by MinCallbackTime"
        );
        assert_eq!(
            dropped.load(Ordering::Acquire),
            0,
            "a MinCallbackTime-throttled frame must NOT increment DroppedArrays"
        );
    }

    #[test]
    fn test_array_callbacks_zero_withholds_downstream_delivery() {
        // ADC-2: NDArrayCallbacks==0 stops downstream NDArray delivery while
        // the plugin still processes and updates its metadata params — C++
        // endProcessCallbacks (NDPluginDriver.cpp:257-265) caches the array and
        // returns before doCallbacksGenericPointer. Distinct from
        // EnableCallbacks, which gates whether the plugin processes at all.
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("ARRAY_CB_TEST").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);
        let port = handle.port_runtime().port_handle().clone();

        // Disable downstream array callbacks.
        port.write_int32_blocking(handle.ndarray_params.array_callbacks, 0, 0)
            .unwrap();
        params_applied(&handle);

        send_array(handle.array_sender(), make_test_array(1));
        // Processing fence: the counter flush follows any downstream publish,
        // so once it reads 1, a delivery that was going to happen already has.
        wait_until("frame 1 to be processed", || {
            port.read_int32_blocking(handle.ndarray_params.array_counter, 0)
                .is_ok_and(|v| v == 1)
        });

        // No downstream delivery.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let got = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_millis(50), downstream_rx.recv()).await
        });
        assert!(
            got.is_err(),
            "NDArrayCallbacks=0 must withhold downstream delivery"
        );
        // But the plugin still processed: ArrayCounter advanced.
        assert_eq!(
            port.read_int32_blocking(handle.ndarray_params.array_counter, 0)
                .unwrap(),
            1,
            "processing (and metadata params) must continue while delivery is off"
        );

        // Re-enable: the next array IS delivered downstream.
        port.write_int32_blocking(handle.ndarray_params.array_callbacks, 0, 1)
            .unwrap();
        params_applied(&handle);
        send_array(handle.array_sender(), make_test_array(2));
        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 2);
    }

    #[test]
    fn test_plugin_output_publishes_compressed_size() {
        // ADC-3: every processed array publishes NDCodec / NDCompressedSize
        // (C++ beginProcessCallbacks NDPluginDriver.cpp:213-214) from the
        // INPUT: an uncompressed input carries an empty codec name and
        // compressedSize == raw bytes, whatever the plugin outputs.
        struct CompressProcessor;
        impl NDPluginProcess for CompressProcessor {
            const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
            const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
            const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
            fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
                let mut out = NDArray::clone(array);
                out.codec = Some(crate::codec::Codec {
                    name: crate::codec::CodecName::JPEG,
                    compressed_size: 7,
                    level: 0,
                    shuffle: 0,
                    compressor: 0,
                    original_data_type: NDDataType::UInt8,
                });
                ProcessResult::arrays(vec![Arc::new(out)])
            }
            fn plugin_type(&self) -> &str {
                "Compress"
            }
        }

        // Uncompressed passthrough: a 4-byte UInt8 array → compressedSize == 4.
        {
            let (ds, _rx) = ndarray_channel("DS_RAW", 10);
            let mut output = NDArrayOutput::new();
            output.add(ds);
            let (handle, _jh) = create_plugin_runtime_with_output(
                &asyn_rs::asyn_record::claim_port("CODEC_RAW").unwrap(),
                PassthroughProcessor,
                1_000_000,
                10,
                0,
                output,
                "",
                test_wiring(),
                DEFAULT_PLUGIN_THREADS,
            );
            enable_callbacks(&handle);
            let port = handle.port_runtime().port_handle().clone();
            send_array(handle.array_sender(), make_test_array(1));
            // The read errors with ParamUndefined until the first flush — treat
            // that as "not yet".
            wait_until(
                "uncompressed output to publish CompressedSize == raw byte count",
                || {
                    port.read_int32_blocking(handle.ndarray_params.compressed_size, 0)
                        .is_ok_and(|v| v == 4)
                },
            );
        }

        // Compressed output of an uncompressed input: still the input's.
        {
            let (ds, _rx) = ndarray_channel("DS_CMP", 10);
            let mut output = NDArrayOutput::new();
            output.add(ds);
            let (handle, _jh) = create_plugin_runtime_with_output(
                &asyn_rs::asyn_record::claim_port("CODEC_CMP").unwrap(),
                CompressProcessor,
                1_000_000,
                10,
                0,
                output,
                "",
                test_wiring(),
                DEFAULT_PLUGIN_THREADS,
            );
            enable_callbacks(&handle);
            let port = handle.port_runtime().port_handle().clone();
            send_array(handle.array_sender(), make_test_array(1));
            wait_until("the frame to publish ArrayCounter", || {
                port.read_int32_blocking(handle.ndarray_params.array_counter, 0)
                    .is_ok_and(|v| v == 1)
            });
            assert_eq!(
                port.read_int32_blocking(handle.ndarray_params.compressed_size, 0)
                    .unwrap(),
                4
            );
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let (data, status) = rt
                .block_on(port.read_octet(handle.ndarray_params.codec, 0, 16))
                .into_parts();
            status.unwrap();
            assert_eq!(data, b"\0");
        }
    }

    #[test]
    fn test_process_plugin_skips_throttled_input() {
        // a MinCallbackTime-throttled frame must NOT be cached as the
        // ProcessPlugin input. After array 1 is processed and array 2 is
        // dropped by the throttle, ProcessPlugin must re-inject array 1
        // (the last *processed* array), not the dropped array 2.
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("PROCESS_THROTTLE_TEST").unwrap(),
            PassthroughProcessor,
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);

        // 10s minimum between callbacks — only the first array is processed.
        handle
            .port_runtime()
            .port_handle()
            .write_float64_blocking(handle.plugin_params.min_callback_time, 0, 10.0)
            .unwrap();
        params_applied(&handle);

        send_array(handle.array_sender(), make_test_array(1));
        send_array(handle.array_sender(), make_test_array(2));

        // Array 1 was processed and emitted; array 2 was throttled out.
        assert_eq!(downstream_rx.blocking_recv().unwrap().unique_id, 1);
        // Fence before the reset below un-gates the throttle: array 2 was
        // refused at the gate on the producer's thread, so nothing of it is
        // still in flight, but array 1 may still be processing.
        params_applied(&handle);

        // ProcessPlugin re-injects the cached input. The cache must still hold
        // array 1, because array 2 never passed the throttle gate. The
        // re-injected array itself is also subject to the throttle, so reset
        // MinCallbackTime to 0 first so the re-injected frame is processed.
        handle
            .port_runtime()
            .port_handle()
            .write_float64_blocking(handle.plugin_params.min_callback_time, 0, 0.0)
            .unwrap();
        // No fence needed: the MinCallbackTime reset and the ProcessPlugin
        // trigger below travel the same FIFO param channel, so the reset is
        // applied before the trigger by construction.
        handle
            .port_runtime()
            .port_handle()
            .write_int32_blocking(handle.plugin_params.process_plugin, 0, 1)
            .unwrap();
        let reprocessed = downstream_rx.blocking_recv().unwrap();
        assert_eq!(
            reprocessed.unique_id, 1,
            "ProcessPlugin must re-inject the last processed array (1), not the throttled array (2)"
        );
    }

    #[test]
    fn test_g3_compressed_array_dropped_on_non_aware_plugin() {
        // G3: a non-compression-aware plugin drops a compressed array.
        let (downstream_sender, mut downstream_rx) = ndarray_channel("DOWNSTREAM", 10);
        let mut output = NDArrayOutput::new();
        output.add(downstream_sender);

        let (handle, _data_jh) = create_plugin_runtime_with_output(
            &asyn_rs::asyn_record::claim_port("G3_TEST").unwrap(),
            PassthroughProcessor, // compression_aware() defaults to false
            1_000_000,
            10,
            0,
            output,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&handle);

        // A compressed array must be dropped, not forwarded.
        let mut compressed = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        compressed.unique_id = 1;
        compressed.codec = Some(crate::codec::Codec {
            name: crate::codec::CodecName::JPEG,
            compressed_size: 16,
            level: 0,
            shuffle: 0,
            compressor: 0,
            original_data_type: NDDataType::UInt8,
        });
        send_array(handle.array_sender(), published(Arc::new(compressed)));

        // An uncompressed array passes through normally.
        send_array(handle.array_sender(), make_test_array(2));

        let r = downstream_rx.blocking_recv().unwrap();
        assert_eq!(
            r.unique_id, 2,
            "compressed array dropped; only the raw array reaches downstream"
        );
    }

    #[test]
    fn test_drop_on_full_increments_dropped_counter() {
        // B1/G1: a slow downstream plugin with a tiny input queue drops arrays
        // when the queue is full; the drop is counted in the plugin's shared
        // DroppedArrays counter rather than back-pressuring the producer.
        struct SlowProcessor;
        impl NDPluginProcess for SlowProcessor {
            const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
            const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
            const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
            fn process_array(&self, _a: &Arc<NDArray>, _p: &NDArrayPool) -> ProcessResult {
                std::thread::sleep(std::time::Duration::from_millis(200));
                ProcessResult::empty()
            }
            fn plugin_type(&self) -> &str {
                "Slow"
            }
        }

        // Downstream plugin with queue size 1 and a slow processor.
        let (downstream_handle, _ds_jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("B1_DOWNSTREAM").unwrap(),
            SlowProcessor,
            1_000_000,
            1,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        enable_callbacks(&downstream_handle);
        let ds_sender = downstream_handle.array_sender().clone();
        let dropped = ds_sender.dropped_arrays_counter().clone();

        // First array is taken by the data loop (now sleeping 200ms); second
        // fills the 1-slot queue; the rest find a full queue → dropped.
        send_array(&ds_sender, make_test_array(1));
        send_array(&ds_sender, make_test_array(2));
        send_array(&ds_sender, make_test_array(3));
        send_array(&ds_sender, make_test_array(4));

        assert!(
            dropped.load(Ordering::Acquire) >= 1,
            "arrays dropped on a full queue must be counted (got {})",
            dropped.load(Ordering::Acquire)
        );
    }

    #[test]
    fn test_cross_width_narrowing_array_read_truncates() {
        // Cross-width integer narrowing array reads must TRUNCATE (wrapping),
        // matching the C cast in C++ NDArrayPool.cpp:387 `convertType`
        //   *pDataOut++ = (dataTypeOut)(*pDataIn++);
        // A C cast `(epicsInt8)(epicsUInt16)300` keeps the low 8 bits == 44.
        // The f64 round-trip in copy_convert would SATURATE (`300.0 as i8`
        // == 127) and diverge from C++ — copy_ccast must be used instead.

        // U16 -> i8: 300 = 0x012C; low byte 0x2C = 44.
        let mut out = [0i8; 1];
        let n = copy_ccast(&[300u16], &mut out);
        assert_eq!(n, 1);
        assert_eq!(out[0], 44, "(epicsInt8)(epicsUInt16)300 == 44 (low 8 bits)");
        // copy_convert would have saturated:
        let mut sat = [0i8; 1];
        copy_convert(&[300u16], &mut sat);
        assert_eq!(sat[0], 127, "f64 round-trip saturates — the wrong behavior");

        // I32 -> i8: 0x1234_5678 -> low byte 0x78 = 120.
        let mut out2 = [0i8; 1];
        copy_ccast(&[0x1234_5678i32], &mut out2);
        assert_eq!(out2[0], 0x78);

        // I32 -> i8: -1 stays -1 (all-ones low byte).
        let mut out3 = [0i8; 1];
        copy_ccast(&[-1i32], &mut out3);
        assert_eq!(out3[0], -1);

        // U16 -> i8: 0x00FF = 255 -> low byte 0xFF reinterpreted as i8 == -1.
        let mut out4 = [0i8; 1];
        copy_ccast(&[255u16], &mut out4);
        assert_eq!(out4[0], -1);

        // I64 -> i32: 0x0000_0001_0000_002A -> low 32 bits == 42.
        let mut out5 = [0i32; 1];
        copy_ccast(&[0x0000_0001_0000_002Ai64], &mut out5);
        assert_eq!(out5[0], 42);

        // U32 -> i16: 70000 = 0x0001_1170 -> low 16 bits 0x1170 == 4464.
        let mut out6 = [0i16; 1];
        copy_ccast(&[70000u32], &mut out6);
        assert_eq!(out6[0], 4464);

        // Same-width sign change still works as a bitwise reinterpret:
        // U8 255 -> i8 -1.
        let mut out7 = [0i8; 1];
        copy_ccast(&[255u8], &mut out7);
        assert_eq!(out7[0], -1);

        // F64 out-of-range -> i32 still routes through copy_convert (the
        // `convert:` arm for float sources). C++ converts float->int with a
        // C cast too, but the runtime keeps the f64 numeric path for float
        // sources; this asserts the integer-narrowing fix did not change the
        // float-source path.
        let mut fout = [0i32; 1];
        copy_convert(&[42.9f64], &mut fout);
        assert_eq!(fout[0], 42, "f64 -> i32 truncates toward zero");
    }

    // ---- ADP-45: scatter overflow-reroute (C++ NDPluginScatter) ----

    /// Run an async body on a throwaway current-thread runtime.
    fn block<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn test_scatter_reroutes_past_full_consumer() {
        // 3 consumers, queue size 1. Pre-fill A so its queue is full; a scatter
        // that would target A must reroute to B (C++ auxStatus=asynOverflow),
        // and the rerouted-away full queue must NOT count a dropped array
        // (driverCallback ignoreQueueFull, NDPluginDriver.cpp:406,433-442).
        let (sa, mut ra) = ndarray_channel("A", 1);
        let (sb, mut rb) = ndarray_channel("B", 1);
        let (sc, _rc) = ndarray_channel("C", 1);
        block(async {
            assert_eq!(
                sa.publish(make_test_array(99)).await,
                PublishOutcome::Delivered
            );
            let senders = vec![sa.clone(), sb.clone(), sc.clone()];
            let mut cursor = 0usize;
            ProcessOutput::scatter_publish(&make_test_array(1), &senders, &mut cursor).await;
            // A rerouted (attempt 0), B delivered (attempt 1): cursor +2.
            assert_eq!(cursor, 2);
            assert_eq!(rb.recv().await.unwrap().unique_id, 1);
            // A still holds only its filler; the rerouted-away drop was not counted.
            assert_eq!(ra.recv().await.unwrap().unique_id, 99);
            assert_eq!(sa.dropped_arrays_counter().load(Ordering::Acquire), 0);
        });
    }

    #[test]
    fn test_scatter_drops_on_last_when_all_full_counts_once() {
        // Both consumers full. The array is dropped on the last node only, and
        // the drop is counted exactly once (C++ sets auxStatus=asynSuccess for
        // the last node so its full queue drops and counts).
        let (sa, mut ra) = ndarray_channel("A", 1);
        let (sb, mut rb) = ndarray_channel("B", 1);
        block(async {
            sa.publish(make_test_array(91)).await;
            sb.publish(make_test_array(92)).await;
            let senders = vec![sa.clone(), sb.clone()];
            let mut cursor = 0usize;
            ProcessOutput::scatter_publish(&make_test_array(7), &senders, &mut cursor).await;
            assert_eq!(cursor, 2); // both attempted
            // A rerouted-away (not counted); B last (dropped, counted once).
            assert_eq!(sa.dropped_arrays_counter().load(Ordering::Acquire), 0);
            assert_eq!(sb.dropped_arrays_counter().load(Ordering::Acquire), 1);
            // Neither queue received frame 7 — both still hold their fillers.
            assert_eq!(ra.recv().await.unwrap().unique_id, 91);
            assert_eq!(rb.recv().await.unwrap().unique_id, 92);
        });
    }

    #[test]
    fn test_scatter_cursor_advances_per_attempt_across_frames() {
        // A is permanently full; B and C are free. Frame 0 reroutes A->B, so
        // the persistent cursor (C++ nextClient_) ends past B. Frame 1 must
        // therefore start at C, NOT back at B: a per-frame cursor would send
        // frame 1 to B; the per-attempt cursor sends it to C.
        let (sa, _ra) = ndarray_channel("A", 1);
        let (sb, mut rb) = ndarray_channel("B", 10);
        let (sc, mut rc) = ndarray_channel("C", 10);
        block(async {
            sa.publish(make_test_array(90)).await; // fill A permanently
            let senders = vec![sa.clone(), sb.clone(), sc.clone()];
            let mut cursor = 0usize;
            ProcessOutput::scatter_publish(&make_test_array(0), &senders, &mut cursor).await;
            assert_eq!(cursor, 2); // A(reroute) + B(deliver)
            assert_eq!(rb.recv().await.unwrap().unique_id, 0);
            ProcessOutput::scatter_publish(&make_test_array(1), &senders, &mut cursor).await;
            assert_eq!(cursor, 3); // C(deliver) on the first attempt
            assert_eq!(rc.recv().await.unwrap().unique_id, 1);
        });
    }

    #[test]
    fn test_scatter_skips_disabled_consumer() {
        // A disabled downstream is unregistered from the interrupt list in C++
        // (setArrayInterrupt(0)) and must not consume a round-robin slot.
        let (sa, mut ra) = ndarray_channel("A", 10);
        let (mut sb, _rb) = ndarray_channel("B", 10);
        let (sc, mut rc) = ndarray_channel("C", 10);
        sb.set_mode_flags(
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        block(async {
            let senders = vec![sa.clone(), sb.clone(), sc.clone()];
            let mut cursor = 0usize;
            // Active set = [A, C] (n=2): frame 0 -> A, frame 1 -> C.
            ProcessOutput::scatter_publish(&make_test_array(0), &senders, &mut cursor).await;
            ProcessOutput::scatter_publish(&make_test_array(1), &senders, &mut cursor).await;
            assert_eq!(ra.recv().await.unwrap().unique_id, 0);
            assert_eq!(rc.recv().await.unwrap().unique_id, 1);
        });
    }

    /// Holds every frame in `process_array` until released, so the input queue
    /// is the only thing absorbing the backlog.
    struct GatedProcessor {
        open: Arc<parking_lot::Mutex<bool>>,
        gate: Arc<parking_lot::Condvar>,
        /// Bumped on entry, ahead of the gate: a frame that has reached a
        /// callback thread at all shows up here even while the gate is shut.
        entered: Arc<std::sync::atomic::AtomicUsize>,
        /// uniqueIds in completion order.
        log: Arc<parking_lot::Mutex<Vec<i32>>>,
    }

    impl GatedProcessor {
        fn new() -> (Self, Arc<std::sync::atomic::AtomicUsize>, GatedControls) {
            let controls = GatedControls {
                open: Arc::new(parking_lot::Mutex::new(false)),
                gate: Arc::new(parking_lot::Condvar::new()),
                log: Arc::new(parking_lot::Mutex::new(Vec::new())),
            };
            let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            (
                Self {
                    open: controls.open.clone(),
                    gate: controls.gate.clone(),
                    entered: entered.clone(),
                    log: controls.log.clone(),
                },
                entered,
                controls,
            )
        }
    }

    struct GatedControls {
        open: Arc<parking_lot::Mutex<bool>>,
        gate: Arc<parking_lot::Condvar>,
        log: Arc<parking_lot::Mutex<Vec<i32>>>,
    }

    impl GatedControls {
        fn release(&self) {
            *self.open.lock() = true;
            self.gate.notify_all();
        }
        fn processed(&self) -> Vec<i32> {
            self.log.lock().clone()
        }
    }

    impl NDPluginProcess for GatedProcessor {
        const ASYN_FLAGS: PortFlags = ASYN_CANBLOCK;
        const INTERFACE_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS;
        const INTERRUPT_MASK: InterfaceMask = InterfaceMask::UNMIGRATED_SUBCLASS_INTERRUPTS;
        fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
            self.entered.fetch_add(1, Ordering::AcqRel);
            let mut open = self.open.lock();
            while !*open {
                self.gate.wait(&mut open);
            }
            drop(open);
            self.log.lock().push(array.unique_id);
            ProcessResult::empty()
        }
        fn plugin_type(&self) -> &str {
            "Gated"
        }
        fn array_callbacks_seed(&self) -> Option<bool> {
            Some(false)
        }
    }

    /// A QueueSize write must resize the input queue, not just the readback.
    ///
    /// C deletes `pToThreadMsgQ_` and news it at the new depth
    /// (NDPluginDriver.cpp:730-733 -> :985), so an operator who raises
    /// QueueSize gets a deeper queue. This port fixed the depth at
    /// construction, so the PV moved and the queue did not — a backlog past
    /// the ORIGINAL depth was still dropped and counted in DroppedArrays while
    /// QueueFree claimed room. That is the observable: same PV writes, same
    /// frames offered, arrays lost.
    #[test]
    fn test_queue_size_write_resizes_the_input_queue() {
        const OFFERED: usize = 20;
        let (processor, _entered, gated) = GatedProcessor::new();
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("QSIZE_TEST").unwrap(),
            processor,
            1_000_000,
            2,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        enable_callbacks(&handle);

        // Raise the depth while nothing is in flight, exactly as an operator
        // would before starting acquisition.
        port.write_int32_blocking(handle.plugin_params.queue_size, 0, 32)
            .unwrap();
        params_applied(&handle);
        assert_eq!(
            port.read_int32_blocking(handle.plugin_params.queue_size, 0)
                .unwrap(),
            32,
            "QueueSize readback did not take"
        );

        // One frame occupies the single worker; the remaining 19 must fit in
        // the queue the operator just asked for. The gate is shut, so nothing
        // drains while they are offered.
        for id in 0..OFFERED as i32 {
            send_array(handle.array_sender(), make_test_array(id));
        }

        gated.release();

        // Let the backlog drain, then count. A frame the queue refused never
        // reaches `process_array` at all.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while gated.processed().len() < OFFERED && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        params_applied(&handle);

        let processed = gated.processed().len();
        let dropped = port
            .read_int32_blocking(handle.plugin_params.dropped_arrays, 0)
            .unwrap();
        assert_eq!(
            processed, OFFERED,
            "{OFFERED} frames offered to a queue the operator set to 32, but only {processed} \
             reached the plugin ({dropped} counted dropped): the write moved the PV, not the queue"
        );
        assert_eq!(dropped, 0, "DroppedArrays should not have moved");
    }

    /// C `driverCallback` republishes QueueFree after every `trySend`, taken
    /// or refused (NDPluginDriver.cpp:446-448), so a backlog behind a busy
    /// callback thread counts QueueFree down to 0 and trips its LOLO.
    #[test]
    fn queue_free_follows_each_enqueue_behind_a_busy_thread() {
        const DEPTH: i32 = 3;
        let (processor, entered, gated) = GatedProcessor::new();
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("QFREE_ENQ").unwrap(),
            processor,
            1_000_000,
            DEPTH,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        let free = || {
            port.read_int32_blocking(handle.plugin_params.queue_use, 0)
                .unwrap()
        };
        enable_callbacks(&handle);

        send_array(handle.array_sender(), make_test_array(0));
        wait_until("the callback thread to hold frame 0", || {
            entered.load(Ordering::Acquire) == 1
        });
        assert_eq!(
            free(),
            DEPTH,
            "a frame taken by an idle thread uses no slot"
        );

        send_array(handle.array_sender(), make_test_array(1));
        wait_until("QueueFree after one queued frame", || free() == DEPTH - 1);
        for id in 2..=DEPTH {
            send_array(handle.array_sender(), make_test_array(id));
        }
        wait_until("QueueFree on a full queue", || free() == 0);
        send_array(handle.array_sender(), make_test_array(DEPTH + 1));
        param_fifo_drained(&handle);
        assert_eq!(free(), 0);
        assert_eq!(
            port.read_int32_blocking(handle.plugin_params.dropped_arrays, 0)
                .unwrap(),
            1
        );

        gated.release();
        wait_until("the backlog to drain", || {
            gated.processed().len() == DEPTH as usize + 1
        });
        wait_until("QueueFree on the drained queue", || free() == DEPTH);
    }

    /// MinCallbackTime exists to relieve queue pressure, so a frame it gates
    /// must occupy no queue slot — and therefore can never push a later frame
    /// out of one. C decides this inside `driverCallback` before `trySend`
    /// (NDPluginDriver.cpp:404-433): a throttled array returns at `:449`
    /// having touched neither the queue nor `DroppedArrays`.
    ///
    /// Deciding it after `recv` instead inverts the observable: the throttled
    /// frames fill the queue, the queue refuses a later one, and the plugin
    /// reports a drop C never reports.
    #[test]
    fn min_callback_time_gate_takes_no_queue_slot() {
        const OFFERED: usize = 20;
        const DEPTH: i32 = 2;
        let (processor, entered, gated) = GatedProcessor::new();
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("MINCB_PRESSURE").unwrap(),
            processor,
            1_000_000,
            DEPTH,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        enable_callbacks(&handle);
        // Long enough that nothing after the first frame can pass the gate,
        // however slowly the test runs.
        port.write_float64_blocking(handle.plugin_params.min_callback_time, 0, 30.0)
            .unwrap();
        params_applied(&handle);

        // Park the single callback thread on frame 0, so from here on the
        // queue is the only thing that could absorb an arrival.
        send_array(handle.array_sender(), make_test_array(0));
        wait_until("frame 0 reached the callback thread", || {
            entered.load(Ordering::Acquire) == 1
        });
        for id in 1..OFFERED as i32 {
            send_array(handle.array_sender(), make_test_array(id));
        }
        gated.release();
        params_applied(&handle);

        assert_eq!(
            gated.processed(),
            vec![0],
            "only the frame that passed the gate is processed"
        );
        assert_eq!(
            port.read_int32_blocking(handle.plugin_params.dropped_arrays, 0)
                .unwrap(),
            0,
            "the throttle gate counts nothing: C reaches DroppedArrays only \
             from the compression check (:390) and a refused trySend (:440)"
        );
        assert_eq!(
            port.read_int32_blocking(handle.plugin_params.queue_use, 0)
                .unwrap(),
            DEPTH,
            "and it consumed no queue slot on the way"
        );
    }

    /// Every compressed array offered to a non-compression-aware plugin is
    /// counted, one per array (NDPluginDriver.cpp:383-394). The overflow
    /// episode cannot silence them: `auxStatus` is only read inside the
    /// `minCallbackTime` branch at `:405`, which the compression gate returns
    /// before ever reaching.
    ///
    /// Letting them into the queue instead collapses the count to the number
    /// of overflow episodes — here 3 rather than 12 — because they then reach
    /// `trySend` and arm the episode cell.
    #[test]
    fn every_compressed_array_counts_against_dropped_arrays() {
        const COMPRESSED: usize = 12;
        fn compressed_array(id: i32) -> Arc<NDArray> {
            let mut a = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
            a.unique_id = id;
            a.codec = Some(crate::codec::Codec {
                name: crate::codec::CodecName::JPEG,
                compressed_size: 16,
                level: 0,
                shuffle: 0,
                compressor: 0,
                original_data_type: NDDataType::UInt8,
            });
            Arc::new(a)
        }
        let (processor, entered, gated) = GatedProcessor::new();
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("COMPR_PRESSURE").unwrap(),
            processor,
            1_000_000,
            2,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        enable_callbacks(&handle);

        // One raw frame parks the single callback thread, so the queue is the
        // only thing that could absorb what follows — the pressure that makes
        // the episode collapse visible.
        send_array(handle.array_sender(), make_test_array(0));
        wait_until("frame 0 reached the callback thread", || {
            entered.load(Ordering::Acquire) == 1
        });
        for id in 1..=COMPRESSED as i32 {
            send_array(handle.array_sender(), published(compressed_array(id)));
        }
        gated.release();
        params_applied(&handle);

        assert_eq!(
            port.read_int32_blocking(handle.plugin_params.dropped_arrays, 0)
                .unwrap(),
            COMPRESSED as i32,
            "one count per compressed array, not one per overflow episode"
        );
        assert_eq!(gated.processed(), vec![0], "and none of them was processed");
    }

    /// ProcessPlugin re-injects through the queue, not around it.
    ///
    /// C hands the cached array to `driverCallback`
    /// (NDPluginDriver.cpp:741) — the detector's own entry point — so the
    /// re-processed frame takes its turn behind whatever is already queued and
    /// is run by a callback thread. Re-injecting on the data loop instead put
    /// it ahead of the backlog and ran it on a thread that is not a callback
    /// thread at all, so with every callback thread busy a client saw the
    /// duplicate arrive *before* frames the detector had already delivered.
    #[test]
    fn test_process_plugin_queues_behind_the_backlog() {
        let (processor, entered, gated) = GatedProcessor::new();
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("PPORDER_TEST").unwrap(),
            processor,
            1_000_000,
            8,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        enable_callbacks(&handle);

        // Frame 1 occupies the single callback thread and is the array
        // ProcessPlugin will re-inject; 2 and 3 pile up behind it.
        send_array(handle.array_sender(), make_test_array(1));
        wait_until("frame 1 reached the callback thread", || {
            entered.load(Ordering::Acquire) == 1
        });
        send_array(handle.array_sender(), make_test_array(2));
        send_array(handle.array_sender(), make_test_array(3));

        port.write_int32_blocking(handle.plugin_params.process_plugin, 0, 1)
            .unwrap();
        param_fifo_drained(&handle);
        assert_eq!(
            entered.load(Ordering::Acquire),
            1,
            "the re-injected frame entered process_array with the only callback \
             thread still busy, so it did not go through the queue"
        );

        gated.release();
        wait_until("every frame drained", || gated.processed().len() == 4);
        params_applied(&handle);
        assert_eq!(
            gated.processed(),
            vec![1, 2, 3, 1],
            "the re-injected frame must arrive after the queued backlog"
        );
    }

    /// A full queue refuses the re-injection and counts it.
    ///
    /// `driverCallback` `trySend`s (NDPluginDriver.cpp:430) and on failure
    /// bumps DroppedArrays (`:433-441`), and ProcessPlugin goes through it like
    /// any array. Running the work inline instead meant the one request C
    /// refuses always succeeded: the operator got an output frame where C
    /// gives them a dropped-array count.
    #[test]
    fn test_process_plugin_is_refused_when_the_queue_is_full() {
        let (processor, entered, gated) = GatedProcessor::new();
        let (handle, _jh) = create_plugin_runtime(
            &asyn_rs::asyn_record::claim_port("PPFULL_TEST").unwrap(),
            processor,
            1_000_000,
            1,
            0,
            "",
            test_wiring(),
            DEFAULT_PLUGIN_THREADS,
        );
        let port = handle.port_runtime().port_handle();
        enable_callbacks(&handle);

        send_array(handle.array_sender(), make_test_array(1));
        wait_until("frame 1 reached the callback thread", || {
            entered.load(Ordering::Acquire) == 1
        });
        // Frame 2 fills the one-deep queue, so there is no room for a
        // re-injection.
        send_array(handle.array_sender(), make_test_array(2));

        port.write_int32_blocking(handle.plugin_params.process_plugin, 0, 1)
            .unwrap();
        param_fifo_drained(&handle);
        assert_eq!(
            entered.load(Ordering::Acquire),
            1,
            "the re-injection was processed even though the queue was full"
        );
        assert_eq!(
            port.read_int32_blocking(handle.plugin_params.dropped_arrays, 0)
                .unwrap(),
            1,
            "a refused re-injection must be counted in DroppedArrays"
        );

        gated.release();
        wait_until("every frame drained", || gated.processed().len() == 2);
        params_applied(&handle);
        assert_eq!(gated.processed(), vec![1, 2]);
    }
}
