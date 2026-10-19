use asyn_rs::port::InterfaceMask;
use std::collections::VecDeque;
use std::sync::Arc;

use ad_core_rs::ndarray::NDArray;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};
use epics_base_rs::calc;
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};

/// Compiled EPICS calc expression wrapper.
///
/// Uses the full epics-base-rs calc engine which supports variables A-L (indices 0-11)
/// plus arithmetic, math functions (ABS, SQRT, LOG, LN, EXP, SIN, COS, MIN, MAX, etc.),
/// comparison, logical, and bitwise operators -- matching the C++ EPICS calc engine.
///
/// For trigger calculations the C++ passes:
///   A=attrValueA, B=attrValueB, C=preTrigger, D=postTrigger, E=currentImage, F=triggered
#[derive(Debug, Clone)]
pub struct CalcExpression {
    compiled: calc::CompiledExpr,
}

impl CalcExpression {
    /// Compile an infix expression string.
    ///
    /// Returns `None` if the expression is invalid.
    pub fn parse(expr: &str) -> Option<CalcExpression> {
        calc::compile(expr)
            .ok()
            .map(|compiled| CalcExpression { compiled })
    }

    /// Evaluate with variables A and B only (legacy 2-variable interface).
    /// Returns the numeric result; nonzero means true for trigger purposes.
    pub fn evaluate(&self, a: f64, b: f64) -> f64 {
        let mut inputs = calc::NumericInputs::new();
        inputs.vars[0] = a; // A
        inputs.vars[1] = b; // B
        calc::eval(&self.compiled, &mut inputs).unwrap_or(0.0)
    }

    /// Evaluate with the full variable set (A through U).
    ///
    /// `vars` is indexed 0=A, 1=B, 2=C, ... up to `CALC_NARGS - 1` = U.
    /// `None` is C `calcPerform` failing.
    pub fn evaluate_vars(&self, vars: &[f64; calc::CALC_NARGS]) -> Option<f64> {
        let mut inputs = calc::NumericInputs::with_vars(*vars);
        calc::eval(&self.compiled, &mut inputs).ok()
    }
}

/// C's trigger inputs: the TriggerA/TriggerB attribute names and the compiled
/// TriggerCalc, run on every untriggered frame (NDPluginCircularBuff.cpp:24-79).
/// `calc` is `None` when postfix failed, which leaves C an empty program that
/// never fires. The default is the "0" the PINI write of an empty TriggerCalc
/// stores (:340).
#[derive(Debug, Clone)]
pub struct TriggerCondition {
    pub attr_a: String,
    pub attr_b: String,
    pub calc: Option<CalcExpression>,
}

impl Default for TriggerCondition {
    fn default() -> Self {
        Self {
            attr_a: String::new(),
            attr_b: String::new(),
            calc: CalcExpression::parse("0"),
        }
    }
}

/// Status of the circular buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferStatus {
    Idle,
    BufferFilling,
    Flushing,
    AcquisitionCompleted,
}

/// Trigger calc inputs and result for one frame, mirroring C's
/// `triggerCalcArgs_[0]`/`[1]` and `calcResult` (NDPluginCircularBuff.cpp:67-78).
#[derive(Debug, Clone, Copy)]
pub struct TriggerValues {
    /// Value of the TriggerA attribute (NaN if absent).
    pub a: f64,
    /// Value of the TriggerB attribute (NaN if absent).
    pub b: f64,
    /// Result of the trigger calc expression; `None` when C `calcPerform`
    /// fails and posts no TriggerCalcVal (:69-74).
    pub calc: Option<f64>,
}

/// The parameter-library assignments C `NDPluginCircularBuff::processCallbacks`
/// makes while handling one frame (NDPluginCircularBuff.cpp:120-203).
///
/// A field is `Some` exactly when C calls `setIntegerParam`/`setStringParam` for
/// it on this frame, and `None` when C leaves the parameter untouched — which is
/// how C freezes `NDCircBuffCurrentImage` at the pre-buffer size for the whole
/// flush (it is assigned only on the pre-trigger branch, `:151`).
///
/// Later assignments in the same frame overwrite earlier ones, exactly as
/// repeated `setIntegerParam` calls do before C's single trailing
/// `callParamCallbacks()`: on a frame that both flushes and completes, clients
/// see only the final `NDCircBuffPostCount = 0`, never the intermediate count.
///
/// [`CircularBuffer::push`] — the state machine that decides the transitions —
/// is the single owner of these values; the processor only maps them onto
/// parameter indices. Reconstructing them from the post-push buffer state is
/// what produced the divergences this type removes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FrameParams {
    /// `NDCircBuffTriggered` — C `:126`/`:132` (every frame that evaluates the
    /// trigger) and `:188`/`:193` (cleared when the sequence ends).
    pub triggered: Option<i32>,
    /// `NDCircBuffCurrentImage` — C `:151`, the pre-buffer size, assigned ONLY
    /// on a pre-trigger frame.
    pub current_image: Option<i32>,
    /// `NDCircBuffPostCount` — C `:168-169` per forwarded post-trigger frame,
    /// and `:189` (reset to 0) when the sequence re-arms.
    pub post_count: Option<i32>,
    /// `NDCircBuffActualTriggerCount` — C `:179-180`, incremented when the
    /// post-trigger count is reached, not when the trigger fires.
    pub actual_trigger_count: Option<i32>,
    /// `NDCircBuffSoftTrigger` — C `:187`, the soft-trigger latch cleared on
    /// re-arm.
    pub soft_trigger: Option<i32>,
    /// `NDCircBuffControl` — C `:186` (re-arm, still 1) and `:194` (the preset
    /// trigger count was reached: C turns acquisition off).
    pub control: Option<i32>,
    /// `NDCircBuffStatus` — C's `setStringParam` calls (`:152-153`, `:159`,
    /// `:190-191`, `:195`).
    pub status: Option<&'static str>,
}

/// Result of pushing a frame: which frames to forward downstream now, whether a
/// capture sequence completed on this push, and the parameter assignments C
/// makes for the frame.
#[derive(Debug, Default)]
pub struct PushResult {
    /// Frames to forward downstream immediately, in order.
    pub forward: Vec<Arc<NDArray>>,
    /// True if the post-trigger count was reached on this push.
    pub sequence_done: bool,
    /// Trigger calc inputs/result when the calc was evaluated this frame
    /// (C `calculateTrigger` path). `None` when already triggered or for a
    /// non-calc trigger condition.
    pub trigger_values: Option<TriggerValues>,
    /// Parameters C assigns for this frame — see [`FrameParams`].
    pub params: FrameParams,
}

/// Circular buffer state for pre/post-trigger capture.
pub struct CircularBuffer {
    /// C `NDCircBuffControl` (`scopeControl`) — the acquisition gate, and the
    /// ONLY thing that admits a frame. C wraps the entire body of
    /// `processCallbacks` in `if (scopeControl) { ... } else { /* nothing */ }`
    /// (NDPluginCircularBuff.cpp:121-203): while it is off, an arriving frame is
    /// not triggered on, not copied, not buffered, and does not advance the
    /// completion test.
    ///
    /// It is a distinct thing from [`Self::status`], which is C's
    /// `NDCircBuffStatus` — a display string. The port previously had no Control
    /// state and inferred "running" from that string, which is what let a
    /// stopped (or never-started) plugin record frames. Starts off: C's param
    /// defaults to 0, so nothing is recorded before a `Control = 1` write.
    control: bool,
    pub(crate) pre_count: usize,
    pub(crate) post_count: usize,
    buffer: VecDeque<Arc<NDArray>>,
    pub(crate) trigger_condition: TriggerCondition,
    triggered: bool,
    /// Number of post-trigger frames forwarded so far for the current trigger.
    post_done: usize,
    /// True once the pre-buffer has been flushed for the current trigger.
    pre_flushed: bool,
    /// Maximum number of triggers before stopping (0 = unlimited).
    preset_trigger_count: usize,
    /// C `actualTriggerCount`: capture sequences *completed* so far — bumped
    /// when the post-trigger count is reached (NDPluginCircularBuff.cpp:179),
    /// not when the trigger fires.
    trigger_count: usize,
    /// C `NDCircBuffFlushOnSoftTrig` (epicsInt32), held raw. The flush decision
    /// is C's `if (flushOn > 0)` (NDPluginCircularBuff.cpp:276) and lives in the
    /// single owner [`CircularBuffer::flushes_on_soft_trigger`] — storing a
    /// pre-digested `bool` cost the sign, and the `!= 0` that produced it made a
    /// negative FlushOnSoftTrig flush on the port and not in C.
    flush_on_soft_trigger: i32,
    /// Current buffer status.
    pub(crate) status: BufferStatus,
}

impl CircularBuffer {
    pub fn new(pre_count: usize, post_count: usize, condition: TriggerCondition) -> Self {
        Self {
            // C's NDCircBuffControl param defaults to 0 — the plugin records
            // nothing until a `Control = 1` write. Call `start()`.
            control: false,
            pre_count,
            post_count,
            buffer: VecDeque::with_capacity(pre_count + 1),
            trigger_condition: condition,
            triggered: false,
            post_done: 0,
            pre_flushed: false,
            // C's constructor seeds PresetTriggerCount 1 (NDPluginCircularBuff.cpp:446).
            preset_trigger_count: 1,
            trigger_count: 0,
            flush_on_soft_trigger: 0,
            status: BufferStatus::Idle,
        }
    }

    /// Set the preset trigger count (0 = unlimited).
    pub fn set_preset_trigger_count(&mut self, count: usize) {
        self.preset_trigger_count = count;
    }

    /// C `actualTriggerCount` — the number of *completed* capture sequences.
    /// Reads one less than the number of triggers fired while a flush is still
    /// in progress; C only increments it at the end of the sequence.
    pub fn trigger_count(&self) -> usize {
        self.trigger_count
    }

    /// Get the current buffer status.
    pub fn status(&self) -> BufferStatus {
        self.status
    }

    /// Store `NDCircBuffFlushOnSoftTrig` as written (C `setIntegerParam`); the
    /// value is interpreted only by [`Self::flushes_on_soft_trigger`].
    pub fn set_flush_on_soft_trigger(&mut self, flush_on: i32) {
        self.flush_on_soft_trigger = flush_on;
    }

    /// C `NDPluginCircularBuff.cpp:276` — `if (flushOn > 0) flushPreBuffer()`.
    /// The ONLY reader of `flush_on_soft_trigger`: negative and zero both mean
    /// "do not flush", so a `caput FlushOnSoftTrig -1` leaves the pre-buffer to
    /// drain lazily with the first post-trigger frame, as in C.
    pub fn flushes_on_soft_trigger(&self) -> bool {
        self.flush_on_soft_trigger > 0
    }

    /// C `writeInt32(NDCircBuffControl, 1)` (NDPluginCircularBuff.cpp:233-254):
    /// rebuild the ring, drop the trigger state, zero the counters, and turn
    /// acquisition on. The only way [`Self::push`] starts admitting frames.
    pub fn start(&mut self) {
        self.reset();
        self.control = true;
        self.status = BufferStatus::BufferFilling;
    }

    /// C `writeInt32(NDCircBuffControl, 0)` (NDPluginCircularBuff.cpp:255-260):
    /// acquisition off. C clears the trigger latches and the displayed image
    /// count but leaves the ring alone, so a restart is a fresh `start()`.
    pub fn stop(&mut self) {
        self.control = false;
        self.triggered = false;
        self.status = BufferStatus::Idle;
    }

    /// C `scopeControl` — is the plugin acquiring? The single gate on admitting
    /// a frame, and the same test C's `writeInt32(NDCircBuffPreTrigger)` uses to
    /// reject a pre-count change (:281-283).
    pub fn is_running(&self) -> bool {
        self.control
    }

    /// Push an array into the circular buffer.
    ///
    /// Mirrors C++ `NDPluginCircularBuff::processCallbacks`: on the frame that
    /// triggers, the pre-buffer is flushed immediately and the triggering
    /// frame is forwarded as the first post-trigger frame; each subsequent
    /// post-trigger frame is forwarded individually. The returned
    /// [`PushResult::forward`] holds the frames to send downstream this call.
    pub fn push(&mut self, array: Arc<NDArray>) -> PushResult {
        let mut result = PushResult::default();

        // C `:121-203` — "Are we running?". EVERYTHING below (the trigger
        // evaluation, the array copy, the pre-buffer add, the flush, the post
        // count, and the completion test) sits inside `if (scopeControl)`, whose
        // else arm is literally `// Currently do nothing`. So a frame arriving
        // while acquisition is off changes no state and forwards nothing — and
        // `Control` is the whole gate: it goes off on a user stop AND when the
        // preset trigger count completes the last sequence (`:194`).
        if !self.control {
            return result;
        }

        // C `:124-134` settles `triggered` for this frame BEFORE the branch: a
        // latched trigger (soft trigger, or a previous frame's) short-circuits
        // the calc; otherwise the calc runs and its outcome is posted.
        if !self.triggered {
            let fired = self.evaluate_trigger(&array, &mut result);
            // C `:132` posts the trigger flag on every frame that evaluated the
            // trigger calc, whether it fired or not.
            result.params.triggered = Some(i32::from(fired));
            if fired {
                // The trigger fires before this frame reaches the pre-buffer, so
                // it becomes the first post-trigger frame — C's triggered branch
                // has no `addToEnd`.
                self.trigger();
            }
        }

        if !self.triggered {
            // C `:142-155` — no trigger yet: the frame joins the pre-trigger ring.
            self.buffer.push_back(array);
            if self.buffer.len() > self.pre_count {
                self.buffer.pop_front();
            }
            // C `:151` posts the ring size — on this branch only, so the value
            // stays frozen at the pre-trigger size for the whole flush.
            result.params.current_image = Some(self.buffer.len() as i32);
            // C `:152-153` only touches the status once the ring is at capacity.
            if self.buffer.len() == self.pre_count {
                result.params.status = Some(if self.pre_count > 0 {
                    "Buffer Wrapping"
                } else {
                    "Dropping frames"
                });
            }
        } else {
            // C `:156-175` — post-trigger: flush the ring once (`:162-166`), then
            // forward this frame and bump the post count (`:168-171`).
            result.params.status = Some("Flushing");
            if !self.pre_flushed {
                result.forward.extend(self.flush_pre_buffer());
            }
            result.forward.push(array);
            self.post_done += 1;
            result.params.post_count = Some(self.post_done as i32);
        }

        // C `:178-197` tests `currentPostCount >= postCount` OUTSIDE the
        // triggered/untriggered branches — on every frame the plugin records.
        // The test therefore also runs on an untriggered frame, where
        // currentPostCount is 0: with postCount == 0 it passes, so C completes a
        // sequence (bumping ActualTriggerCount and re-arming) once per *running*
        // frame, triggered or not. Evaluating it only after a post-trigger push
        // is the R8-71 divergence.
        if self.post_done >= self.post_count {
            self.complete_sequence(&mut result);
        }

        result
    }

    /// C `:131` / `calculateTrigger` — does this frame fire the trigger?
    /// Records the calc inputs and result in `result` as a side effect, exactly
    /// as C posts TriggerAVal/BVal/CalcVal on every evaluated frame
    /// (NDPluginCircularBuff.cpp:67-78), regardless of the outcome.
    fn evaluate_trigger(&self, array: &NDArray, result: &mut PushResult) -> bool {
        let t = &self.trigger_condition;
        let attr = |name: &str| {
            array
                .attributes
                .get(name)
                .and_then(|a| a.value.as_f64())
                .unwrap_or(f64::NAN)
        };
        let (a, b) = (attr(&t.attr_a), attr(&t.attr_b));
        // A=attrValueA, B=attrValueB, C=preTrigger, D=postTrigger,
        // E=currentImage, F=triggered (:43-48).
        let mut vars = [0.0f64; calc::CALC_NARGS];
        vars[0] = a;
        vars[1] = b;
        vars[2] = self.pre_count as f64;
        vars[3] = self.post_count as f64;
        vars[4] = self.buffer.len() as f64;
        vars[5] = if self.triggered { 1.0 } else { 0.0 };
        let calc = t.calc.as_ref().and_then(|e| e.evaluate_vars(&vars));
        result.trigger_values = Some(TriggerValues { a, b, calc });
        // C fires only when the result is a finite non-zero (:77 `!isnan &&
        // !isinf && != 0`); a missing attribute is epicsNAN and never fires.
        calc.is_some_and(|c| c.is_finite() && c != 0.0)
    }

    /// C `flushPreBuffer()` (NDPluginCircularBuff.cpp:208-218): drain the
    /// pre-trigger ring and hand every frame downstream, in order.
    ///
    /// The single owner of the drain. Both callers go through it — the first
    /// post-trigger frame (`:162-166`) and the soft-trigger write when
    /// FlushOnSoftTrig > 0 (`:276-277`) — and whichever runs second finds the
    /// ring already empty, exactly as C does.
    /// The oldest ringed frame (tests: the ring holds the input itself).
    #[cfg(test)]
    fn pre_buffer_front(&self) -> Option<Arc<NDArray>> {
        self.buffer.front().cloned()
    }

    fn flush_pre_buffer(&mut self) -> Vec<Arc<NDArray>> {
        self.pre_flushed = true;
        self.buffer.drain(..).collect()
    }

    /// Finalize a completed post-trigger sequence (C++
    /// `currentPostCount >= postCount` branch, NDPluginCircularBuff.cpp:178-197):
    /// advance status / trigger bookkeeping and signal completion.
    fn complete_sequence(&mut self, result: &mut PushResult) {
        self.triggered = false;
        self.pre_flushed = false;
        self.post_done = 0;
        // C increments actualTriggerCount HERE — when the post-trigger count is
        // reached — not when the trigger fires (`:179-180`). During a flush the
        // count still reads the number of *completed* sequences.
        self.trigger_count += 1;
        result.params.actual_trigger_count = Some(self.trigger_count as i32);
        if self.preset_trigger_count > 0 && self.trigger_count >= self.preset_trigger_count {
            // C `:193-195`: preset reached — clear the trigger and turn
            // acquisition off (NDCircBuffControl = 0). Turning it off here is
            // what stops the NEXT frame: the gate at the top of `push` is the
            // same one a user stop clears, so completion needs no separate
            // "already completed" branch.
            self.control = false;
            self.status = BufferStatus::AcquisitionCompleted;
            result.params.triggered = Some(0);
            result.params.control = Some(0);
            result.params.status = Some("Acquisition Completed");
        } else {
            // C `:186-191`: re-arm for the next trigger — the soft-trigger
            // latch and the post count are cleared, control stays on.
            self.status = BufferStatus::BufferFilling;
            result.params.control = Some(1);
            result.params.soft_trigger = Some(0);
            result.params.triggered = Some(0);
            result.params.post_count = Some(0);
            result.params.status = Some(if self.pre_count > 0 {
                "Buffer filling"
            } else {
                "Dropping frames"
            });
        }
        result.sequence_done = true;
    }

    /// External trigger.
    pub fn trigger(&mut self) {
        // The trigger only means anything while acquiring — C evaluates and
        // latches `triggered` inside `if (scopeControl)`, so a soft trigger
        // arriving with acquisition off (never started, user-stopped, or the
        // preset count completed) cannot start a flush.
        if !self.control {
            return;
        }

        self.triggered = true;
        self.post_done = 0;
        // The pre-buffer is flushed lazily on the first post-trigger push so
        // the frames stream out in order with the post-trigger frames.
        self.pre_flushed = false;
        self.status = BufferStatus::Flushing;
    }

    pub fn is_triggered(&self) -> bool {
        self.triggered
    }

    pub fn pre_buffer_len(&self) -> usize {
        self.buffer.len()
    }

    /// Drop the ring and every counter. Leaves acquisition OFF — `start()` is
    /// the one entry point that turns it on.
    pub fn reset(&mut self) {
        self.control = false;
        self.buffer.clear();
        self.triggered = false;
        self.post_done = 0;
        self.pre_flushed = false;
        self.trigger_count = 0;
        self.status = BufferStatus::Idle;
    }
}

// --- New CircularBuffProcessor (NDPluginProcess-based) ---

/// CircularBuff processor: maintains ring buffer state, forwards the
/// pre-trigger flush and each post-trigger frame downstream as they arrive.
#[derive(Default)]
struct CBParamIndices {
    control: Option<usize>,
    status: Option<usize>,
    trigger_a: Option<usize>,
    trigger_b: Option<usize>,
    trigger_a_val: Option<usize>,
    trigger_b_val: Option<usize>,
    trigger_calc: Option<usize>,
    trigger_calc_val: Option<usize>,
    pre_trigger: Option<usize>,
    post_trigger: Option<usize>,
    current_image: Option<usize>,
    post_count: Option<usize>,
    soft_trigger: Option<usize>,
    triggered: Option<usize>,
    preset_trigger_count: Option<usize>,
    actual_trigger_count: Option<usize>,
    flush_on_soft_trigger: Option<usize>,
}

/// The ring, its trigger inputs included.
struct CircularBuffState {
    buffer: CircularBuffer,
}

pub struct CircularBuffProcessor {
    state: Mutex<CircularBuffState>,
    params: CBParamIndices,
    /// C `maxBuffers_`, the `maxBuffers` argument of `NDCircularBuffConfigure`
    /// (NDPluginCircularBuff.cpp:407). Bounds the accepted pre-count: C
    /// rejects `preCount > maxBuffers_ - 1` (:284).
    max_buffers: i32,
}

impl CircularBuffProcessor {
    pub fn new(
        pre_count: usize,
        post_count: usize,
        condition: TriggerCondition,
        max_buffers: i32,
    ) -> Self {
        Self {
            state: Mutex::new(CircularBuffState {
                buffer: CircularBuffer::new(pre_count, post_count, condition),
            }),
            params: CBParamIndices::default(),
            max_buffers,
        }
    }

    pub fn trigger(&self) {
        self.state.lock().buffer.trigger();
    }

    /// Turn acquisition on, as a `Control = 1` write does. Until this is called
    /// the plugin records nothing — C's `NDCircBuffControl` starts at 0 and
    /// `processCallbacks` does nothing while it is off.
    pub fn start(&self) {
        self.state.lock().buffer.start();
    }

    /// Turn acquisition off, as a `Control = 0` write does.
    pub fn stop(&self) {
        self.state.lock().buffer.stop();
    }

    pub fn buffer(&self) -> MappedMutexGuard<'_, CircularBuffer> {
        MutexGuard::map(self.state.lock(), |s| &mut s.buffer)
    }
}

impl NDPluginProcess for CircularBuffProcessor {
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ad_core_rs::plugin::runtime::ASYN_NONE;
    // NDPluginCircularBuff.cpp:400-401, the same mask for both.
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::INT32_ARRAY
        .or(InterfaceMask::FLOAT64_ARRAY)
        .or(InterfaceMask::GENERIC_POINTER);
    const INTERRUPT_MASK: InterfaceMask = Self::INTERFACE_MASK;

    /// C `NDPluginCircularBuff::writeInt32` drops the base class status
    /// (`status = (asynStatus) callParamCallbacks();`, NDPluginCircularBuff.cpp:303): a refused NDPluginDriver write keeps
    /// its clamp and succeeds.
    fn validate_int32(
        &self,
        reason: usize,
        _addr: i32,
        value: i32,
        verdict: ad_core_rs::plugin::runtime::WriteVerdict<i32>,
        params: &mut ad_core_rs::plugin::runtime::WritePort<'_>,
    ) -> ad_core_rs::plugin::runtime::WriteOutcome<i32> {
        use ad_core_rs::plugin::runtime::{WriteOutcome, WriteVerdict};

        let int = |i: Option<usize>| {
            i.and_then(|i| params.get_int32_param(i, 0).ok())
                .unwrap_or(0)
        };
        let ints = |out: WriteOutcome<i32>, set: &[(Option<usize>, i32)]| {
            set.iter().fold(out, |out, &(i, v)| match i {
                Some(i) => out.set_int32(i, 0, v),
                None => out,
            })
        };
        let status = |out: WriteOutcome<i32>, s: &str| match self.params.status {
            Some(i) => out.set_octet(i, 0, s),
            None => out,
        };
        let out = WriteOutcome::from(verdict.succeed());
        let p = &self.params;
        if Some(reason) == p.control {
            if value == 1 {
                // :248-253, with the pre-count C reads from the param.
                let out = ints(
                    out,
                    &[
                        (p.soft_trigger, 0),
                        (p.triggered, 0),
                        (p.post_count, 0),
                        (p.actual_trigger_count, 0),
                    ],
                );
                let filling = if int(p.pre_trigger) > 0 {
                    "Buffer filling"
                } else {
                    "Dropping frames"
                };
                status(out, filling)
            } else {
                // :257-260
                let out = ints(
                    out,
                    &[(p.soft_trigger, 0), (p.triggered, 0), (p.current_image, 0)],
                );
                status(out, "Acquisition Stopped")
            }
        } else if Some(reason) == p.soft_trigger {
            // :271 latches unconditionally; a 0 write disarms here (CBUG-B11,
            // see `on_param_change`).
            if value != 0 {
                ints(out, &[(p.triggered, 1)])
            } else {
                out
            }
        } else if Some(reason) == p.pre_trigger {
            // :281-288: a refused pre-count is never stored, and the write
            // still succeeds through `status = callParamCallbacks()`.
            let refused = if int(p.control) != 0 {
                Some("Stop acquisition to set pre-count")
            } else if value > self.max_buffers - 1 {
                Some("Pre-count too high")
            } else if value < 0 {
                Some("Invalid pre-count value")
            } else {
                None
            };
            match refused {
                Some(msg) => status(
                    WriteOutcome::from(WriteVerdict::Accept(int(p.pre_trigger))).settled(),
                    msg,
                ),
                None => out,
            }
        } else {
            out
        }
    }
    /// C `NDPluginCircularBuff::writeOctet(NDCircBuffTriggerCalc)` stores "0"
    /// for an empty expression and fails the write with postfix's -1 for a bad
    /// one (NDPluginCircularBuff.cpp:337-349, :362).
    fn validate_octet(
        &self,
        reason: usize,
        _addr: i32,
        value: &str,
        verdict: ad_core_rs::plugin::runtime::WriteVerdict<String>,
        _params: &asyn_rs::port::PortDriverBase,
    ) -> ad_core_rs::plugin::runtime::WriteOutcome<String> {
        use ad_core_rs::plugin::runtime::WriteVerdict;
        if Some(reason) != self.params.trigger_calc {
            return verdict.into();
        }
        if value.is_empty() {
            return WriteVerdict::Accept("0".to_string()).into();
        }
        match calc::compile(value) {
            Ok(_) => verdict.into(),
            Err(e) => ad_core_rs::plugin::runtime::WriteOutcome::from(WriteVerdict::Refuse {
                keep: value.to_string(),
                message: format!(
                    "NDPluginCircularBuff:writeOctet: status=-1, function={reason}, value={value}"
                ),
            })
            .print(
                asyn_rs::trace::TraceMask::ERROR,
                format!(
                    "NDPluginCircularBuff::writeOctet error processing infix expression={value}, error={}\n",
                    calc::calc_error_str(e.code()).unwrap_or("(null)")
                ),
            ),
        }
    }

    fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        use ad_core_rs::plugin::runtime::ParamUpdate;

        // C copies the frame into its own pool so the driver's array can be
        // released (NDPluginCircularBuff.cpp:137); a shared immutable Arc
        // needs no copy, the ring just holds a reference.
        let mut state = self.state.lock();
        let push_result = state.buffer.push(Arc::clone(array));

        // The buffer reports exactly the parameters C assigns for this frame
        // (see `FrameParams`); the processor only maps them onto indices. A
        // `None` field is a parameter C leaves alone — emitting a value for it
        // is what froze CurrentImage at 0 during a flush and posted
        // ActualTriggerCount a whole sequence early.
        let mut updates = Vec::new();
        let p = &push_result.params;
        if let (Some(idx), Some(s)) = (self.params.status, p.status) {
            // C NDCircBuffStatus is asynOctet (NDPluginCircularBuff.cpp:411).
            updates.push(ParamUpdate::octet(idx, s.to_string()));
        }
        for (index, value) in [
            (self.params.triggered, p.triggered),
            (self.params.current_image, p.current_image),
            (self.params.post_count, p.post_count),
            (self.params.actual_trigger_count, p.actual_trigger_count),
            (self.params.soft_trigger, p.soft_trigger),
            (self.params.control, p.control),
        ] {
            if let (Some(idx), Some(v)) = (index, value) {
                updates.push(ParamUpdate::int32(idx, v));
            }
        }
        // C posts the trigger calc inputs/result each evaluated frame
        // (NDPluginCircularBuff.cpp:67-78).
        if let Some(tv) = push_result.trigger_values {
            if let Some(idx) = self.params.trigger_a_val {
                updates.push(ParamUpdate::float64(idx, tv.a));
            }
            if let Some(idx) = self.params.trigger_b_val {
                updates.push(ParamUpdate::float64(idx, tv.b));
            }
            if let (Some(idx), Some(calc)) = (self.params.trigger_calc_val, tv.calc) {
                updates.push(ParamUpdate::float64(idx, calc));
            }
        }

        // Stream frames downstream as the C++ plugin does: pre-buffer frames
        // are flushed at the trigger and each post-trigger frame is forwarded
        // immediately, rather than being withheld until the sequence ends.
        // C posts them with doCallbacksGenericPointer, never through
        // endProcessCallbacks (NDPluginCircularBuff.cpp:171, :212-214), so
        // NDArrayCallbacks does not stop them.
        let mut result = ProcessResult::sink(updates);
        result.addressed = push_result.forward.into_iter().map(|a| (0, a)).collect();
        result
    }

    fn plugin_type(&self) -> &str {
        "NDPluginCircularBuff"
    }

    /// NDPluginCircularBuff.cpp:453
    fn array_callbacks_seed(&self) -> Option<bool> {
        Some(true)
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        use asyn_rs::param::ParamType;
        base.create_param("CIRC_BUFF_CONTROL", ParamType::Int32)?;
        // C NDCircBuffStatus is asynParamOctet (NDPluginCircularBuff.cpp:411);
        // the db binds it to a stringin/asynOctetRead record.
        base.create_param("CIRC_BUFF_STATUS", ParamType::Octet)?;
        base.create_param("CIRC_BUFF_TRIGGER_A", ParamType::Octet)?;
        base.create_param("CIRC_BUFF_TRIGGER_B", ParamType::Octet)?;
        base.create_param("CIRC_BUFF_TRIGGER_A_VAL", ParamType::Float64)?;
        base.create_param("CIRC_BUFF_TRIGGER_B_VAL", ParamType::Float64)?;
        base.create_param("CIRC_BUFF_TRIGGER_CALC", ParamType::Octet)?;
        base.create_param("CIRC_BUFF_TRIGGER_CALC_VAL", ParamType::Float64)?;
        base.create_param("CIRC_BUFF_PRE_TRIGGER", ParamType::Int32)?;
        base.create_param("CIRC_BUFF_POST_TRIGGER", ParamType::Int32)?;
        base.create_param("CIRC_BUFF_CURRENT_IMAGE", ParamType::Int32)?;
        base.create_param("CIRC_BUFF_POST_COUNT", ParamType::Int32)?;
        base.create_param("CIRC_BUFF_SOFT_TRIGGER", ParamType::Int32)?;
        base.create_param("CIRC_BUFF_TRIGGERED", ParamType::Int32)?;
        base.create_param("CIRC_BUFF_PRESET_TRIGGER_COUNT", ParamType::Int32)?;
        base.create_param("CIRC_BUFF_ACTUAL_TRIGGER_COUNT", ParamType::Int32)?;
        base.create_param("CIRC_BUFF_FLUSH_ON_SOFTTRIGGER", ParamType::Int32)?;

        self.params.control = base.find_param("CIRC_BUFF_CONTROL");
        self.params.status = base.find_param("CIRC_BUFF_STATUS");
        self.params.trigger_a = base.find_param("CIRC_BUFF_TRIGGER_A");
        self.params.trigger_b = base.find_param("CIRC_BUFF_TRIGGER_B");
        self.params.trigger_a_val = base.find_param("CIRC_BUFF_TRIGGER_A_VAL");
        self.params.trigger_b_val = base.find_param("CIRC_BUFF_TRIGGER_B_VAL");
        self.params.trigger_calc = base.find_param("CIRC_BUFF_TRIGGER_CALC");
        self.params.trigger_calc_val = base.find_param("CIRC_BUFF_TRIGGER_CALC_VAL");
        self.params.pre_trigger = base.find_param("CIRC_BUFF_PRE_TRIGGER");
        self.params.post_trigger = base.find_param("CIRC_BUFF_POST_TRIGGER");
        self.params.current_image = base.find_param("CIRC_BUFF_CURRENT_IMAGE");
        self.params.post_count = base.find_param("CIRC_BUFF_POST_COUNT");
        self.params.soft_trigger = base.find_param("CIRC_BUFF_SOFT_TRIGGER");
        self.params.triggered = base.find_param("CIRC_BUFF_TRIGGERED");
        self.params.preset_trigger_count = base.find_param("CIRC_BUFF_PRESET_TRIGGER_COUNT");
        self.params.actual_trigger_count = base.find_param("CIRC_BUFF_ACTUAL_TRIGGER_COUNT");
        self.params.flush_on_soft_trigger = base.find_param("CIRC_BUFF_FLUSH_ON_SOFTTRIGGER");

        // The params C's constructor seeds (NDPluginCircularBuff.cpp:432-449),
        // read from the buffer that acts on them.
        if let Some(idx) = self.params.status {
            base.set_string_param(idx, 0, "Idle")?;
        }
        let p = &self.params;
        let state = self.state.lock();
        let b = &state.buffer;
        for (idx, v) in [
            (p.current_image, b.buffer.len() as i32),
            (p.post_count, b.post_done as i32),
            (p.control, b.control as i32),
            (p.pre_trigger, b.pre_count as i32),
            (p.post_trigger, b.post_count as i32),
            (p.preset_trigger_count, b.preset_trigger_count as i32),
            (p.actual_trigger_count, b.trigger_count as i32),
            (p.flush_on_soft_trigger, b.flush_on_soft_trigger),
        ] {
            if let Some(idx) = idx {
                base.set_int32_param(idx, 0, v)?;
            }
        }
        Ok(())
    }

    fn on_param_change(
        &self,
        reason: usize,
        params: &ad_core_rs::plugin::runtime::PluginParamSnapshot,
    ) -> ad_core_rs::plugin::runtime::ParamChangeResult {
        use ad_core_rs::plugin::runtime::{ParamChangeResult, ParamChangeValue};

        let mut state = self.state.lock();
        if Some(reason) == self.params.control {
            // The params this write sets are `validate_int32`'s; here only the
            // ring follows. Start rebuilds it, stop turns `scopeControl` off so
            // `push` admits nothing until the next Control=1.
            if params.value.as_i32() == 1 {
                state.buffer.start();
            } else {
                state.buffer.stop();
            }
        } else if Some(reason) == self.params.pre_trigger {
            // Only a pre-count `validate_int32` accepted reaches the ring.
            state.buffer.pre_count = params.value.as_i32() as usize;
        } else if Some(reason) == self.params.post_trigger {
            state.buffer.post_count = params.value.as_i32().max(0) as usize;
        } else if Some(reason) == self.params.preset_trigger_count {
            state
                .buffer
                .set_preset_trigger_count(params.value.as_i32().max(0) as usize);
        } else if Some(reason) == self.params.flush_on_soft_trigger {
            state
                .buffer
                .set_flush_on_soft_trigger(params.value.as_i32());
        } else if Some(reason) == self.params.soft_trigger {
            // The write's job, beyond storing the parameter, is to make the soft
            // trigger take effect IMMEDIATELY — latch Triggered and flush the ring
            // — instead of waiting for the next frame, which `process_array` would
            // do anyway from the stored level (C `NDPluginCircularBuff.cpp:124-125`:
            // `if (softTrigger) triggered = 1;`, re-asserted every frame).
            //
            // DEVIATION from C, deliberate — CBUG-B11. C's writeInt32 arm
            // (NDPluginCircularBuff.cpp:266-278) stores `value` (:268) and then
            // never tests it: it latches NDCircBuffTriggered = 1 (:271) and
            // flushes (:276-278) UNCONDITIONALLY. So `caput SoftTrigger 0` — the
            // natural way to disarm between acquisitions, and what an
            // autosave/PINI restore writes at boot — fires the trigger on C
            // exactly like `caput SoftTrigger 1`. 0 unambiguously means "not
            // triggered" in this plugin's own vocabulary: the Control on and
            // Control off paths both disarm by writing SoftTrigger = 0 alongside
            // Triggered = 0 (:248-249, :257-258), and processCallbacks only
            // asserts the trigger `if (softTrigger)`.
            //
            // Still a deviation, re-verified 2026-08-25: ADCore #599 is NOT in
            // `origin/master` (tip `7380981d`, refs fetched 2026-07-20), and the
            // fix `9e495eac` exists only on the local branch
            // `fix/circbuff-softtrigger-zero-disarms`. Unlike CBUG-B5/B6/B7/B8/B10,
            // which asyn merged, C still has this one.
            //
            // Writing 0 is therefore a no-op here, which is exactly "stops
            // asserting the trigger" — the same level semantics processCallbacks
            // has. It does not clear an already-latched Triggered: C clears that
            // only on a Control transition, and a latch set by the ATTRIBUTE
            // trigger condition is not this parameter's to cancel.
            if params.value.as_i32() != 0 {
                state.buffer.trigger();
                // C `:273-277`: when FlushOnSoftTrig > 0 the pre-buffer is flushed
                // from the write itself, not lazily on the next frame — the ring
                // reaches the downstream plugins before any post-trigger frame.
                if state.buffer.flushes_on_soft_trigger() {
                    let flushed = state.buffer.flush_pre_buffer();
                    if !flushed.is_empty() {
                        let mut result = ParamChangeResult::empty();
                        result.addressed = flushed.into_iter().map(|a| (0, a)).collect();
                        return result;
                    }
                }
            }
        } else if let ParamChangeValue::Octet(s) = &params.value {
            let trigger = &mut state.buffer.trigger_condition;
            if Some(reason) == self.params.trigger_a {
                trigger.attr_a = s.clone();
            } else if Some(reason) == self.params.trigger_b {
                trigger.attr_b = s.clone();
            } else if Some(reason) == self.params.trigger_calc {
                trigger.calc = CalcExpression::parse(s);
            }
        }

        ParamChangeResult::updates(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `status = (asynStatus) callParamCallbacks();` (NDPluginCircularBuff.cpp:303).
    #[test]
    fn a_refused_base_write_succeeds_as_c_write_int32_does() {
        use ad_core_rs::plugin::runtime::WriteVerdict;
        let proc = CircularBuffProcessor::new(2, 1, TriggerCondition::default(), 100);
        let mut base = asyn_rs::port::PortDriverBase::new(
            "T",
            1,
            Default::default(),
            asyn_rs::port::InterfaceMask::UNMIGRATED_ALL,
            asyn_rs::port::InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let clamp = WriteVerdict::Refuse {
            keep: 1,
            message: String::new(),
        };
        assert_eq!(
            proc.validate_int32(
                0,
                0,
                98,
                clamp,
                &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base)
            )
            .verdict,
            WriteVerdict::Accept(1)
        );
    }

    #[test]
    fn test_register_params_seeds_the_c_constructor_values() {
        // Every value NDPluginCircularBuff.cpp:432-449 sets, for the IOC's
        // 100/100 buffer; the triggers and calc stay undefined.
        use asyn_rs::port::InterfaceMask;
        use asyn_rs::port::{PortDriverBase, PortFlags};
        let mut base = PortDriverBase::new(
            "CB_INIT",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let mut proc = CircularBuffProcessor::new(100, 100, TriggerCondition::default(), 10);
        proc.register_params(&mut base).unwrap();
        let idx = |name: &str| base.find_param(name).unwrap();
        let int = |name: &str| base.get_int32_param(idx(name), 0);
        for (name, v) in [
            ("CIRC_BUFF_CURRENT_IMAGE", 0),
            ("CIRC_BUFF_POST_COUNT", 0),
            ("CIRC_BUFF_CONTROL", 0),
            ("CIRC_BUFF_PRE_TRIGGER", 100),
            ("CIRC_BUFF_POST_TRIGGER", 100),
            ("CIRC_BUFF_PRESET_TRIGGER_COUNT", 1),
            ("CIRC_BUFF_ACTUAL_TRIGGER_COUNT", 0),
            ("CIRC_BUFF_FLUSH_ON_SOFTTRIGGER", 0),
        ] {
            assert_eq!(int(name).unwrap(), v, "{name}");
        }
        assert_eq!(
            base.get_string_param(idx("CIRC_BUFF_STATUS"), 0).unwrap(),
            b"Idle"
        );
        for name in ["CIRC_BUFF_SOFT_TRIGGER", "CIRC_BUFF_TRIGGERED"] {
            assert!(int(name).is_err(), "{name}");
        }
    }

    #[test]
    fn test_register_params_reads_the_buffer_it_seeds_from() {
        use asyn_rs::port::InterfaceMask;
        use asyn_rs::port::{PortDriverBase, PortFlags};
        let mut base = PortDriverBase::new(
            "CB_INIT2",
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let mut proc = CircularBuffProcessor::new(5, 7, TriggerCondition::default(), 10);
        proc.register_params(&mut base).unwrap();
        let int = |name: &str| {
            base.get_int32_param(base.find_param(name).unwrap(), 0)
                .unwrap()
        };
        assert_eq!(int("CIRC_BUFF_PRE_TRIGGER"), 5);
        assert_eq!(int("CIRC_BUFF_POST_TRIGGER"), 7);
    }
    use ad_core_rs::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
    use ad_core_rs::ndarray::{NDDataType, NDDimension};

    fn make_array(id: i32) -> Arc<NDArray> {
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = id;
        Arc::new(arr)
    }

    fn make_array_with_attr(id: i32, attr_val: f64) -> Arc<NDArray> {
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = id;
        arr.attributes.add(NDAttribute::new_static(
            "trigger",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Float64(attr_val),
        ));
        Arc::new(arr)
    }

    fn make_array_with_attrs(id: i32, a_val: f64, b_val: f64) -> Arc<NDArray> {
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = id;
        arr.attributes.add(NDAttribute::new_static(
            "attr_a",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Float64(a_val),
        ));
        arr.attributes.add(NDAttribute::new_static(
            "attr_b",
            "",
            NDAttrSource::Driver,
            NDAttrValue::Float64(b_val),
        ));
        Arc::new(arr)
    }

    #[test]
    fn test_pre_trigger_buffering() {
        let mut cb = CircularBuffer::new(3, 2, TriggerCondition::default());
        cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)

        for i in 0..5 {
            cb.push(make_array(i));
        }
        // Pre-buffer should hold last 3
        assert_eq!(cb.pre_buffer_len(), 3);
    }

    /// D7: a completed sequence must leave the plugin holding no clones of the
    /// frames it has already forwarded. `captured` accumulated a second
    /// `Arc<NDArray>` per frame that nothing in the production path ever
    /// drained, so an armed plugin with PreCount/PostCount in the thousands and
    /// multi-MB frames sat on roughly double the configured buffer depth until
    /// the next trigger.
    #[test]
    fn a_completed_sequence_retains_no_forwarded_frames() {
        let mut cb = CircularBuffer::new(2, 2, TriggerCondition::default());
        cb.start();

        let pre = make_array(1);
        cb.push(Arc::clone(&pre));
        cb.trigger();

        let post = make_array(2);
        let r1 = cb.push(Arc::clone(&post));
        let r2 = cb.push(make_array(3));
        assert!(r2.sequence_done);

        // Downstream has taken delivery of both.
        drop(r1);
        drop(r2);

        assert_eq!(
            Arc::strong_count(&pre),
            1,
            "the flushed pre-trigger frame must not be retained after forwarding"
        );
        assert_eq!(
            Arc::strong_count(&post),
            1,
            "the forwarded post-trigger frame must not be retained"
        );
    }

    #[test]
    fn test_external_trigger() {
        let mut cb = CircularBuffer::new(2, 2, TriggerCondition::default());
        cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)

        cb.push(make_array(1));
        cb.push(make_array(2));
        cb.push(make_array(3));
        // Pre-buffer: [2, 3]

        cb.trigger();
        assert!(cb.is_triggered());

        // First post-trigger push flushes the pre-buffer and forwards frame 4.
        let r1 = cb.push(make_array(4));
        assert!(!r1.sequence_done);
        let ids1: Vec<_> = r1.forward.iter().map(|a| a.unique_id).collect();
        assert_eq!(ids1, vec![2, 3, 4]); // 2 pre + frame 4

        // Second post-trigger push forwards frame 5 and completes.
        let r2 = cb.push(make_array(5));
        assert!(r2.sequence_done);
        let ids2: Vec<_> = r2.forward.iter().map(|a| a.unique_id).collect();
        assert_eq!(ids2, vec![5]);
    }

    #[test]
    fn test_post_count_zero_no_underflow() {
        // Regression: post_count == 0 must complete the sequence on the first
        // post-trigger frame instead of underflowing the post counter.
        let mut cb = CircularBuffer::new(2, 0, TriggerCondition::default());
        cb.set_preset_trigger_count(0); // unlimited triggers
        cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)
        cb.push(make_array(1));
        cb.push(make_array(2));
        cb.trigger();
        assert!(cb.is_triggered());

        // First frame after the trigger: pre-buffer flushed + this frame,
        // and the sequence completes immediately (postCount == 0).
        let r = cb.push(make_array(3));
        assert!(r.sequence_done);
        let ids: Vec<_> = r.forward.iter().map(|a| a.unique_id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert!(!cb.is_triggered());
        assert_eq!(cb.status(), BufferStatus::BufferFilling);

        // No panic / no 2^64 capture; further frames just fill the pre-buffer.
        // They are not forwarded — but with postCount == 0 each of them still
        // *completes* a sequence, because C evaluates `currentPostCount >=
        // postCount` outside the triggered branches (NDPluginCircularBuff.cpp:178).
        // The `!r2.sequence_done` this test used to assert was invented; C's
        // value is `true` (R8-71, covered by
        // `test_post_count_zero_completes_on_untriggered_frame`).
        let r2 = cb.push(make_array(4));
        assert!(r2.sequence_done);
        assert!(r2.forward.is_empty());
    }

    #[test]
    fn test_post_count_zero_completes_on_untriggered_frame() {
        // R8-71. C tests `currentPostCount >= postCount` OUTSIDE the
        // triggered/untriggered branches (NDPluginCircularBuff.cpp:178, inside
        // `if (pArrayCpy)`), so with postCount == 0 the test passes on an
        // *untriggered* frame too (currentPostCount is 0 there): C completes a
        // sequence — bumping ActualTriggerCount and re-arming — once per running
        // frame, without forwarding it.
        let mut cb = CircularBuffer::new(2, 0, TriggerCondition::default());
        cb.set_preset_trigger_count(0); // unlimited triggers
        cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)

        for (n, id) in (1..=3).enumerate() {
            let r = cb.push(make_array(id));
            // Untriggered: the frame joins the pre-buffer, nothing is forwarded.
            assert!(r.forward.is_empty(), "frame {id} must not be forwarded");
            // ...yet the sequence completes (C `:178-191`).
            assert!(r.sequence_done, "frame {id} must complete a sequence");
            assert_eq!(r.params.actual_trigger_count, Some(n as i32 + 1));
            assert_eq!(cb.trigger_count(), n + 1);
            // C `:186-189` re-arms: control stays on, latches and post count clear.
            assert_eq!(r.params.control, Some(1));
            assert_eq!(r.params.soft_trigger, Some(0));
            assert_eq!(r.params.triggered, Some(0));
            assert_eq!(r.params.post_count, Some(0));
            assert_eq!(r.params.status, Some("Buffer filling"));
        }
        // C never clears the ring on re-arm, so the pre-buffer keeps filling
        // across the completed sequences: 3 frames, preCount 2 → last two held.
        assert_eq!(cb.pre_buffer_len(), 2);

        // Boundary: postCount > 0 — the same untriggered frame must NOT complete,
        // because currentPostCount (0) < postCount.
        let mut cb = CircularBuffer::new(2, 1, TriggerCondition::default());
        cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)
        let r = cb.push(make_array(1));
        assert!(!r.sequence_done);
        assert_eq!(r.params.actual_trigger_count, None);
        assert_eq!(cb.trigger_count(), 0);
    }

    #[test]
    fn test_post_count_zero_untriggered_frames_reach_preset_trigger_count() {
        // R8-71, preset boundary: with postCount == 0 the per-frame completion
        // also drives ActualTriggerCount into PresetTriggerCount, so C stops
        // acquisition (Control = 0) after `preset` untriggered frames
        // (NDPluginCircularBuff.cpp:181-196).
        let mut cb = CircularBuffer::new(2, 0, TriggerCondition::default());
        cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)
        cb.set_preset_trigger_count(2);

        let r1 = cb.push(make_array(1));
        assert!(r1.sequence_done);
        assert_eq!(r1.params.actual_trigger_count, Some(1));
        assert_eq!(cb.status(), BufferStatus::BufferFilling);

        let r2 = cb.push(make_array(2));
        assert_eq!(r2.params.actual_trigger_count, Some(2));
        // Preset reached — C `:193-195`: Triggered = 0, Control = 0.
        assert_eq!(r2.params.control, Some(0));
        assert_eq!(r2.params.status, Some("Acquisition Completed"));
        assert_eq!(cb.status(), BufferStatus::AcquisitionCompleted);
    }

    #[test]
    fn test_attribute_trigger_post_count_zero() {
        // post_count == 0 with an attribute trigger: the triggering frame is
        // forwarded and the sequence completes on the same push.
        let mut cb = CircularBuffer::new(
            1,
            0,
            TriggerCondition {
                attr_a: "trigger".into(),
                attr_b: String::new(),
                calc: CalcExpression::parse("A>=5"),
            },
        );
        cb.set_preset_trigger_count(0); // unlimited triggers
        cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)
        cb.push(make_array_with_attr(1, 1.0));
        let r = cb.push(make_array_with_attr(2, 9.0));
        assert!(r.sequence_done);
        let ids: Vec<_> = r.forward.iter().map(|a| a.unique_id).collect();
        assert_eq!(ids, vec![1, 2]); // 1 pre + triggering frame
        assert!(!cb.is_triggered());
    }

    #[test]
    fn test_attribute_trigger() {
        let mut cb = CircularBuffer::new(
            1,
            2,
            TriggerCondition {
                attr_a: "trigger".into(),
                attr_b: String::new(),
                calc: CalcExpression::parse("A>=5"),
            },
        );
        cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)

        cb.push(make_array_with_attr(1, 1.0));
        cb.push(make_array_with_attr(2, 2.0));
        assert!(!cb.is_triggered());

        // This should trigger (attr >= 5.0); triggering frame is first post-trigger
        let r3 = cb.push(make_array_with_attr(3, 5.0));
        assert!(cb.is_triggered());
        // Pre-buffer (id=2) flushed + triggering frame (id=3) forwarded now.
        let ids3: Vec<_> = r3.forward.iter().map(|a| a.unique_id).collect();
        assert_eq!(ids3, vec![2, 3]);

        let r4 = cb.push(make_array(4));
        assert!(r4.sequence_done);
        let ids4: Vec<_> = r4.forward.iter().map(|a| a.unique_id).collect();
        assert_eq!(ids4, vec![4]);
    }

    // --- New tests ---

    #[test]
    fn test_calc_trigger() {
        // Expression: "A>5" — trigger when attribute A exceeds 5
        let expr = CalcExpression::parse("A>5").unwrap();
        let mut cb = CircularBuffer::new(
            1,
            2,
            TriggerCondition {
                attr_a: "attr_a".into(),
                attr_b: "attr_b".into(),
                calc: Some(expr),
            },
        );
        cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)

        let mut forwarded: Vec<i32> = Vec::new();
        let mut record = |r: PushResult| {
            forwarded.extend(r.forward.iter().map(|a| a.unique_id));
            r.sequence_done
        };

        // A=3, should not trigger
        record(cb.push(make_array_with_attrs(1, 3.0, 0.0)));
        assert!(!cb.is_triggered());

        // A=6, should trigger; triggering frame is first post-trigger
        record(cb.push(make_array_with_attrs(2, 6.0, 0.0)));
        assert!(cb.is_triggered());

        assert!(record(cb.push(make_array(3))));

        // 1 pre (id=1) + 2 post (id=2 triggering frame + id=3)
        assert_eq!(forwarded, vec![1, 2, 3]);
    }

    #[test]
    fn test_calc_trigger_values_surface() {
        // Regression for ADP-41: the Calc path must surface A, B, and the calc
        // result so the processor can post TriggerAVal/BVal/CalcVal.
        let expr = CalcExpression::parse("A+B").unwrap();
        // post_count=3 so the triggering frame does not finish the sequence and
        // frame 2 stays in the flushing branch.
        let mut cb = CircularBuffer::new(
            2,
            3,
            TriggerCondition {
                attr_a: "attr_a".into(),
                attr_b: "attr_b".into(),
                calc: Some(expr),
            },
        );
        cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)

        // Frame with A=3, B=4 → calc=7 (nonzero → triggers).
        let r = cb.push(make_array_with_attrs(1, 3.0, 4.0));
        let tv = r.trigger_values.expect("calc path surfaces trigger values");
        assert_eq!(tv.a, 3.0);
        assert_eq!(tv.b, 4.0);
        assert_eq!(tv.calc, Some(7.0));

        // Once triggered, the calc is not re-evaluated (C calculateTrigger is
        // skipped while triggered), so no trigger values this frame.
        let r2 = cb.push(make_array(2));
        assert!(r2.trigger_values.is_none());
    }

    #[test]
    fn test_calc_trigger_values_nan_when_attr_absent() {
        // C posts NaN for a missing trigger attribute (triggerCalcArgs_ default
        // epicsNAN); the calc of "A" with A absent is NaN.
        let expr = CalcExpression::parse("A").unwrap();
        let mut cb = CircularBuffer::new(
            2,
            1,
            TriggerCondition {
                attr_a: "missing_a".into(),
                attr_b: "missing_b".into(),
                calc: Some(expr),
            },
        );
        cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)
        let r = cb.push(make_array(1));
        let tv = r.trigger_values.expect("calc path surfaces trigger values");
        assert!(tv.a.is_nan());
        assert!(tv.b.is_nan());
        assert!(tv.calc.unwrap().is_nan());
    }

    /// NDPluginCircularBuff.cpp:67-78: every untriggered frame posts
    /// TriggerAVal/BVal, and TriggerCalcVal only when `calcPerform` succeeds.
    /// An attribute name alone never fires; only the calc does.
    #[test]
    fn the_calc_is_the_only_trigger_as_c_calculate_trigger_does() {
        let push = |calc: Option<CalcExpression>| {
            let mut cb = CircularBuffer::new(
                2,
                2,
                TriggerCondition {
                    attr_a: "attr_a".into(),
                    attr_b: String::new(),
                    calc,
                },
            );
            cb.start();
            let r = cb.push(make_array_with_attrs(1, 9.0, 0.0));
            (r.trigger_values.unwrap(), cb.is_triggered())
        };
        // The default "0" posts CalcVal 0 and never fires, whatever A is.
        let (tv, fired) = push(TriggerCondition::default().calc);
        assert_eq!((tv.a, tv.calc, fired), (9.0, Some(0.0), false));
        assert!(tv.b.is_nan());
        // A failed postfix is an empty program: no CalcVal, no trigger.
        let (tv, fired) = push(None);
        assert_eq!((tv.a, tv.calc, fired), (9.0, None, false));
    }

    #[test]
    fn test_calc_trigger_skips_nan_and_inf_results() {
        // C fires only on a finite non-zero calc result
        // (NDPluginCircularBuff.cpp:77 `!isnan && !isinf && != 0`). A NaN or Inf
        // result must NOT trigger, even though `NaN != 0.0` and `Inf != 0.0` are
        // both true in Rust. Expression "A" surfaces the injected value directly.
        // post_count = 2 so a single triggering push does not immediately
        // complete the sequence and reset the triggered flag.
        let push_calc = |val: f64| {
            let expr = CalcExpression::parse("A").unwrap();
            let mut cb = CircularBuffer::new(
                2,
                2,
                TriggerCondition {
                    attr_a: "attr_a".into(),
                    attr_b: "attr_b".into(),
                    calc: Some(expr),
                },
            );
            cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)
            cb.push(make_array_with_attrs(1, val, 0.0));
            cb.is_triggered()
        };
        // NaN and ±Inf results must not trigger.
        assert!(!push_calc(f64::NAN));
        assert!(!push_calc(f64::INFINITY));
        assert!(!push_calc(f64::NEG_INFINITY));
        // A finite non-zero result still triggers (the guard does not suppress
        // a valid trigger); a finite zero still does not.
        assert!(push_calc(1.0));
        assert!(!push_calc(0.0));
    }

    #[test]
    fn test_calc_expression_parse() {
        // Simple comparison
        let expr = CalcExpression::parse("A>5").unwrap();
        assert_eq!(expr.evaluate(6.0, 0.0), 1.0);
        assert_eq!(expr.evaluate(4.0, 0.0), 0.0);
        assert_eq!(expr.evaluate(5.0, 0.0), 0.0); // not >=

        // Greater-or-equal
        let expr = CalcExpression::parse("A>=5").unwrap();
        assert_eq!(expr.evaluate(5.0, 0.0), 1.0);
        assert_eq!(expr.evaluate(4.9, 0.0), 0.0);

        // Logical AND with two variables
        let expr = CalcExpression::parse("A>3&&B<10").unwrap();
        assert_eq!(expr.evaluate(4.0, 5.0), 1.0);
        assert_eq!(expr.evaluate(2.0, 5.0), 0.0);
        assert_eq!(expr.evaluate(4.0, 15.0), 0.0);

        // Parenthesized OR
        let expr = CalcExpression::parse("(A>10)||(B>10)").unwrap();
        assert_eq!(expr.evaluate(11.0, 0.0), 1.0);
        assert_eq!(expr.evaluate(0.0, 11.0), 1.0);
        assert_eq!(expr.evaluate(0.0, 0.0), 0.0);

        // Not-equal
        let expr = CalcExpression::parse("A!=0").unwrap();
        assert_eq!(expr.evaluate(1.0, 0.0), 1.0);
        assert_eq!(expr.evaluate(0.0, 0.0), 0.0);

        // Equality
        let expr = CalcExpression::parse("A==B").unwrap();
        assert_eq!(expr.evaluate(5.0, 5.0), 1.0);
        assert_eq!(expr.evaluate(5.0, 6.0), 0.0);

        // Not operator
        let expr = CalcExpression::parse("!A").unwrap();
        assert_eq!(expr.evaluate(0.0, 0.0), 1.0);
        assert_eq!(expr.evaluate(1.0, 0.0), 0.0);

        // The full EPICS calc engine treats single '=' as equality (like '==')
        // and single '&' as bitwise AND, so both are valid expressions.
        let expr = CalcExpression::parse("A=5").unwrap();
        assert_eq!(expr.evaluate(5.0, 0.0), 1.0);
        assert_eq!(expr.evaluate(4.0, 0.0), 0.0);

        let expr = CalcExpression::parse("A&B").unwrap();
        // 3 & 1 = 1 (bitwise AND)
        assert_eq!(expr.evaluate(3.0, 1.0), 1.0);

        // Test math functions supported by the full calc engine
        let expr = CalcExpression::parse("ABS(A)").unwrap();
        assert_eq!(expr.evaluate(-5.0, 0.0), 5.0);

        let expr = CalcExpression::parse("SQRT(A)").unwrap();
        assert!((expr.evaluate(9.0, 0.0) - 3.0).abs() < 1e-10);

        let expr = CalcExpression::parse("A+B").unwrap();
        assert_eq!(expr.evaluate(3.0, 4.0), 7.0);

        let expr = CalcExpression::parse("A-B").unwrap();
        assert_eq!(expr.evaluate(10.0, 3.0), 7.0);

        let expr = CalcExpression::parse("A*B").unwrap();
        assert_eq!(expr.evaluate(3.0, 4.0), 12.0);

        let expr = CalcExpression::parse("A/B").unwrap();
        assert_eq!(expr.evaluate(12.0, 4.0), 3.0);

        // Test variables C through F using evaluate_vars
        let expr = CalcExpression::parse("A>5&&C>0").unwrap();
        let mut vars = [0.0f64; calc::CALC_NARGS];
        vars[0] = 6.0; // A
        vars[2] = 1.0; // C
        assert_eq!(expr.evaluate_vars(&vars), Some(1.0));
        vars[2] = 0.0; // C=0 should fail the condition
        assert_eq!(expr.evaluate_vars(&vars), Some(0.0));

        // Invalid expression returns None
        assert!(CalcExpression::parse("@@@").is_none());
    }

    #[test]
    fn test_preset_trigger_count() {
        let mut cb = CircularBuffer::new(1, 1, TriggerCondition::default());
        cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)
        cb.set_preset_trigger_count(2);

        // C's Control=1 write sets the status string to "Buffer filling"
        // straight away (NDPluginCircularBuff.cpp:252-253) — it does not wait
        // for a frame.
        assert_eq!(cb.status(), BufferStatus::BufferFilling);

        cb.push(make_array(1));
        assert_eq!(cb.status(), BufferStatus::BufferFilling);

        // First trigger. C's actualTriggerCount does not move until the
        // post-trigger count is reached (NDPluginCircularBuff.cpp:179).
        cb.trigger();
        assert_eq!(cb.trigger_count(), 0);
        assert_eq!(cb.status(), BufferStatus::Flushing);

        let done = cb.push(make_array(2));
        assert!(done.sequence_done);
        assert_eq!(cb.trigger_count(), 1); // counted at completion
        assert_eq!(cb.status(), BufferStatus::BufferFilling); // back to filling after first capture

        // Refill buffer
        cb.push(make_array(3));

        // Second trigger — completing it reaches the preset count
        cb.trigger();
        assert_eq!(cb.trigger_count(), 1);
        assert_eq!(cb.status(), BufferStatus::Flushing);

        let done = cb.push(make_array(4));
        assert!(done.sequence_done);
        assert_eq!(cb.trigger_count(), 2);
        assert_eq!(cb.status(), BufferStatus::AcquisitionCompleted);

        // Further frames should be ignored
        let done = cb.push(make_array(5));
        assert!(!done.sequence_done);
        assert_eq!(cb.status(), BufferStatus::AcquisitionCompleted);

        // Further triggers should be ignored
        cb.trigger();
        assert_eq!(cb.trigger_count(), 2); // unchanged
    }

    /// A registered CB port: the processor (queue size 10, so `maxBuffers_-1`
    /// is 9) and its param base.
    fn cb_port() -> (CircularBuffProcessor, asyn_rs::port::PortDriverBase) {
        let mut proc = CircularBuffProcessor::new(3, 1, TriggerCondition::default(), 10);
        let mut base = asyn_rs::port::PortDriverBase::new(
            "T",
            1,
            Default::default(),
            asyn_rs::port::InterfaceMask::UNMIGRATED_ALL,
            asyn_rs::port::InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        proc.register_params(&mut base).unwrap();
        (proc, base)
    }

    fn cb_write(
        proc: &CircularBuffProcessor,
        base: &mut asyn_rs::port::PortDriverBase,
        name: &str,
        value: i32,
    ) -> ad_core_rs::plugin::runtime::WriteOutcome<i32> {
        use ad_core_rs::plugin::runtime::WriteVerdict;
        let reason = base.find_param(name).unwrap();
        proc.validate_int32(
            reason,
            0,
            value,
            WriteVerdict::Accept(value),
            &mut ad_core_rs::plugin::runtime::WritePort::new(base),
        )
    }

    fn side_int(
        out: &ad_core_rs::plugin::runtime::WriteOutcome<i32>,
        base: &asyn_rs::port::PortDriverBase,
        name: &str,
    ) -> Option<i32> {
        use ad_core_rs::plugin::runtime::SideParam;
        let target = base.find_param(name).unwrap();
        out.side.iter().find_map(|p| match p {
            SideParam::Int32 { reason, value, .. } if *reason == target => Some(*value),
            _ => None,
        })
    }

    fn side_status(out: &ad_core_rs::plugin::runtime::WriteOutcome<i32>) -> Option<&str> {
        use ad_core_rs::plugin::runtime::SideParam;
        out.side.iter().find_map(|p| match p {
            SideParam::Octet { value, .. } => Some(value.as_str()),
            _ => None,
        })
    }

    #[test]
    fn control_sets_its_params_inside_the_write_as_c_write_int32_does() {
        let (proc, mut base) = cb_port();
        // :257-260 `setIntegerParam(NDCircBuffCurrentImage, 0);
        // setStringParam(NDCircBuffStatus, "Acquisition Stopped");`
        let out = cb_write(&proc, &mut base, "CIRC_BUFF_CONTROL", 0);
        assert_eq!(side_int(&out, &base, "CIRC_BUFF_CURRENT_IMAGE"), Some(0));
        assert_eq!(side_status(&out), Some("Acquisition Stopped"));
        assert!(!out.settled);

        // :252-253 `preCount ? "Buffer filling" : "Dropping frames"`
        let pre = base.find_param("CIRC_BUFF_PRE_TRIGGER").unwrap();
        base.set_int32_param(pre, 0, 0).unwrap();
        let out = cb_write(&proc, &mut base, "CIRC_BUFF_CONTROL", 1);
        assert_eq!(
            side_int(&out, &base, "CIRC_BUFF_ACTUAL_TRIGGER_COUNT"),
            Some(0)
        );
        assert_eq!(side_status(&out), Some("Dropping frames"));
        base.set_int32_param(pre, 0, 3).unwrap();
        let out = cb_write(&proc, &mut base, "CIRC_BUFF_CONTROL", 1);
        assert_eq!(side_status(&out), Some("Buffer filling"));

        // :271 `setIntegerParam(NDCircBuffTriggered, 1);`
        let out = cb_write(&proc, &mut base, "CIRC_BUFF_SOFT_TRIGGER", 1);
        assert_eq!(side_int(&out, &base, "CIRC_BUFF_TRIGGERED"), Some(1));
    }

    #[test]
    fn trigger_calc_is_checked_inside_the_write_as_c_write_octet_does() {
        use ad_core_rs::plugin::runtime::WriteVerdict;
        let (proc, base) = cb_port();
        let calc = base.find_param("CIRC_BUFF_TRIGGER_CALC").unwrap();
        let write =
            |v: &str| proc.validate_octet(calc, 0, v, WriteVerdict::Accept(v.to_string()), &base);
        // :340-341 `setStringParam(NDCircBuffTriggerCalc, DEFAULT_TRIGGER_CALC);`
        assert_eq!(write("").verdict, WriteVerdict::Accept("0".to_string()));
        assert_eq!(
            write("A>1").verdict,
            WriteVerdict::Accept("A>1".to_string())
        );
        // :345-349 and :362: postfix's -1 is the write's status.
        let out = write("A>");
        assert_eq!(
            out.verdict,
            WriteVerdict::Refuse {
                keep: "A>".to_string(),
                message: format!(
                    "NDPluginCircularBuff:writeOctet: status=-1, function={calc}, value=A>"
                ),
            }
        );
        assert_eq!(
            out.trace[0].1,
            "NDPluginCircularBuff::writeOctet error processing infix expression=A>, error=Incomplete expression, operand missing\n"
        );
    }

    #[test]
    fn a_refused_pre_count_is_not_stored_as_c_write_int32_does() {
        use ad_core_rs::plugin::runtime::WriteVerdict;
        let (proc, mut base) = cb_port();
        let (pre, control) = (
            base.find_param("CIRC_BUFF_PRE_TRIGGER").unwrap(),
            base.find_param("CIRC_BUFF_CONTROL").unwrap(),
        );
        base.set_int32_param(pre, 0, 3).unwrap();
        let refused = |out: ad_core_rs::plugin::runtime::WriteOutcome<i32>, msg| {
            assert_eq!(out.verdict, WriteVerdict::Accept(3));
            assert_eq!(side_status(&out), Some(msg));
            assert!(out.settled);
        };
        // :285-288 "Pre-count too high" above maxBuffers_-1, "Invalid pre-count value" below 0.
        refused(
            cb_write(&proc, &mut base, "CIRC_BUFF_PRE_TRIGGER", 10),
            "Pre-count too high",
        );
        refused(
            cb_write(&proc, &mut base, "CIRC_BUFF_PRE_TRIGGER", -1),
            "Invalid pre-count value",
        );
        // :291 `status = (asynStatus) setIntegerParam(function, value);`
        let out = cb_write(&proc, &mut base, "CIRC_BUFF_PRE_TRIGGER", 9);
        assert_eq!(out.verdict, WriteVerdict::Accept(9));
        assert_eq!(side_status(&out), None);
        // :282-283 `if (scopeControl) { setStringParam(NDCircBuffStatus,
        // "Stop acquisition to set pre-count");`
        base.set_int32_param(control, 0, 1).unwrap();
        refused(
            cb_write(&proc, &mut base, "CIRC_BUFF_PRE_TRIGGER", 7),
            "Stop acquisition to set pre-count",
        );
    }

    #[test]
    fn test_frame_status_strings() {
        // ADP-40: NDCircBuffStatus is an Octet string and `push` now owns it —
        // the exact C strings, on exactly the frames C calls setStringParam.
        // Filling below capacity: C makes no setStringParam call at all.
        let mut cb = CircularBuffer::new(2, 2, TriggerCondition::default());
        cb.set_preset_trigger_count(0); // unlimited triggers
        cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)
        assert_eq!(cb.push(make_array(1)).params.status, None);
        // Ring reaches capacity → "Buffer Wrapping" on this and every later
        // filling frame.
        assert_eq!(
            cb.push(make_array(2)).params.status,
            Some("Buffer Wrapping")
        );
        assert_eq!(
            cb.push(make_array(3)).params.status,
            Some("Buffer Wrapping")
        );
        // Flushing frame (forwarded, sequence not done).
        cb.trigger();
        assert_eq!(cb.push(make_array(4)).params.status, Some("Flushing"));
        // Sequence completes with more triggers allowed → back to filling.
        assert_eq!(cb.push(make_array(5)).params.status, Some("Buffer filling"));

        // preCount == 0: the ring is always "at capacity", so C reports dropped
        // frames both while filling and on completion.
        let mut cb = CircularBuffer::new(0, 1, TriggerCondition::default());
        cb.set_preset_trigger_count(0); // unlimited triggers
        cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)
        assert_eq!(
            cb.push(make_array(1)).params.status,
            Some("Dropping frames")
        );
        cb.trigger();
        assert_eq!(
            cb.push(make_array(2)).params.status,
            Some("Dropping frames")
        );

        // Preset trigger count reached → "Acquisition Completed".
        let mut cb = CircularBuffer::new(2, 1, TriggerCondition::default());
        cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)
        cb.set_preset_trigger_count(1);
        cb.trigger();
        assert_eq!(
            cb.push(make_array(1)).params.status,
            Some("Acquisition Completed")
        );
    }

    #[test]
    fn test_post_count_posted_per_flushed_frame() {
        // R8-65: C increments currentPostCount and posts NDCircBuffPostCount on
        // every forwarded post-trigger frame (NDPluginCircularBuff.cpp:168-169),
        // then resets it to 0 when the sequence re-arms (:189). The port cached
        // the param index but never emitted an update, so PostCount_RBV read 0
        // forever.
        let mut cb = CircularBuffer::new(2, 3, TriggerCondition::default());
        cb.set_preset_trigger_count(0); // unlimited triggers
        cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)
        // Pre-trigger frames touch neither the post count...
        assert_eq!(cb.push(make_array(1)).params.post_count, None);
        assert_eq!(cb.push(make_array(2)).params.post_count, None);

        cb.trigger();
        assert_eq!(cb.push(make_array(3)).params.post_count, Some(1));
        assert_eq!(cb.push(make_array(4)).params.post_count, Some(2));
        // Third post-trigger frame completes the sequence: C posts the count (3)
        // and then 0 from the re-arm branch, so the client only ever sees 0.
        assert_eq!(cb.push(make_array(5)).params.post_count, Some(0));

        // Re-armed: the next sequence counts from 1 again.
        cb.trigger();
        assert_eq!(cb.push(make_array(6)).params.post_count, Some(1));
    }

    #[test]
    fn test_post_count_survives_acquisition_completed() {
        // On the "Acquisition Completed" branch C does NOT reset PostCount
        // (:193-195 has no setIntegerParam(NDCircBuffPostCount, 0)), so the
        // final count stays visible after the preset trigger count is reached.
        let mut cb = CircularBuffer::new(1, 2, TriggerCondition::default());
        cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)
        cb.set_preset_trigger_count(1);
        cb.trigger();
        assert_eq!(cb.push(make_array(1)).params.post_count, Some(1));
        let done = cb.push(make_array(2));
        assert_eq!(done.params.post_count, Some(2), "final count, not reset");
        assert_eq!(done.params.status, Some("Acquisition Completed"));
        assert_eq!(done.params.control, Some(0), "C turns acquisition off");
    }

    #[test]
    fn test_current_image_frozen_during_flush() {
        // R8-65 sibling: C assigns NDCircBuffCurrentImage only on the
        // pre-trigger branch (`:151`), so during a flush the value stays frozen
        // at the pre-buffer size it had when the trigger fired. The port posted
        // `pre_buffer_len()` on every frame — and the flush drains the ring, so
        // it posted 0 for the whole capture.
        let mut cb = CircularBuffer::new(3, 2, TriggerCondition::default());
        cb.set_preset_trigger_count(0); // unlimited triggers
        cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)
        assert_eq!(cb.push(make_array(1)).params.current_image, Some(1));
        assert_eq!(cb.push(make_array(2)).params.current_image, Some(2));

        cb.trigger();
        // Flushing frames leave the parameter alone — no update, so the record
        // holds the last pre-trigger size (2).
        let r1 = cb.push(make_array(3));
        assert_eq!(cb.pre_buffer_len(), 0, "the flush drained the ring");
        assert_eq!(r1.params.current_image, None);
        assert_eq!(cb.push(make_array(4)).params.current_image, None);

        // Back to filling: the ring size is reported again, from 1.
        assert_eq!(cb.push(make_array(5)).params.current_image, Some(1));
    }

    #[test]
    fn test_actual_trigger_count_increments_at_sequence_completion() {
        // R8-65 sibling: C increments actualTriggerCount when the post-trigger
        // count is reached (`:179-180`), not when the trigger fires. The port
        // bumped it inside trigger() and posted it every frame, so
        // ActualTriggerCount_RBV stepped a whole sequence early.
        let mut cb = CircularBuffer::new(1, 2, TriggerCondition::default());
        cb.set_preset_trigger_count(0); // unlimited triggers
        cb.start(); // C: NDCircBuffControl = 1 (the plugin only records while acquiring)
        cb.push(make_array(1));
        assert_eq!(cb.trigger_count(), 0);

        cb.trigger();
        assert_eq!(cb.trigger_count(), 0, "the trigger alone completes nothing");

        // First post-trigger frame: still mid-sequence, no count update.
        let r1 = cb.push(make_array(2));
        assert_eq!(r1.params.actual_trigger_count, None);
        assert_eq!(cb.trigger_count(), 0);

        // Second (last) post-trigger frame: the sequence completes and the count
        // moves to 1, together with the re-arm parameters C writes.
        let r2 = cb.push(make_array(3));
        assert!(r2.sequence_done);
        assert_eq!(r2.params.actual_trigger_count, Some(1));
        assert_eq!(cb.trigger_count(), 1);
        assert_eq!(r2.params.soft_trigger, Some(0), "C clears the soft latch");
        assert_eq!(r2.params.triggered, Some(0));
        assert_eq!(r2.params.control, Some(1), "still acquiring");
    }

    /// The ring holds the frame it was handed; C copies through its pool
    /// (NDPluginCircularBuff.cpp:137) only so the driver's array can be
    /// released, which a shared `Arc` does not need.
    #[test]
    fn the_ringed_frame_is_the_input_arc_itself() {
        use ad_core_rs::ndarray::{NDDataType, NDDimension};

        let p = CircularBuffProcessor::new(1, 1, TriggerCondition::default(), 100);
        p.buffer().start();
        let pool = NDArrayPool::new(0);
        let input = Arc::new(NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8));
        p.process_array(&input, &pool);
        assert!(Arc::ptr_eq(&p.buffer().pre_buffer_front().unwrap(), &input));
        assert_eq!(pool.num_alloc_buffers(), 0);
    }

    #[test]
    fn test_processor_emits_the_frame_params() {
        // The processor maps `FrameParams` onto the registered indices: a
        // flushing frame must emit POST_COUNT and leave CURRENT_IMAGE alone.
        use ad_core_rs::ndarray::{NDDataType, NDDimension};
        use ad_core_rs::plugin::runtime::ParamUpdate;

        let mut p = CircularBuffProcessor::new(2, 2, TriggerCondition::default(), 100);
        p.buffer().set_preset_trigger_count(0); // unlimited triggers
        p.buffer().start(); // C: NDCircBuffControl = 1
        p.params.current_image = Some(11);
        p.params.post_count = Some(13);
        p.params.actual_trigger_count = Some(16);
        let pool = NDArrayPool::new(0);
        let frame = || NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        let int32s = |r: &ProcessResult| -> Vec<(usize, i32)> {
            r.param_updates
                .iter()
                .filter_map(|u| match u {
                    ParamUpdate::Int32 { reason, value, .. } => Some((*reason, *value)),
                    _ => None,
                })
                .collect()
        };

        // Pre-trigger frame: CURRENT_IMAGE=1, no POST_COUNT.
        let r = p.process_array(&Arc::new(frame()), &pool);
        assert!(int32s(&r).contains(&(11, 1)));
        assert!(!int32s(&r).iter().any(|(reason, _)| *reason == 13));

        // Flushing frame: POST_COUNT=1 and NO CURRENT_IMAGE update (the pre-fix
        // processor posted CURRENT_IMAGE=0 here and never posted POST_COUNT).
        p.trigger();
        let r = p.process_array(&Arc::new(frame()), &pool);
        assert!(int32s(&r).contains(&(13, 1)), "POST_COUNT posted per frame");
        assert!(
            !int32s(&r).iter().any(|(reason, _)| *reason == 11),
            "CURRENT_IMAGE frozen during the flush"
        );
        assert!(
            !int32s(&r).iter().any(|(reason, _)| *reason == 16),
            "ActualTriggerCount only moves at completion"
        );

        // Completing frame: ACTUAL_TRIGGER_COUNT=1, POST_COUNT reset to 0.
        let r = p.process_array(&Arc::new(frame()), &pool);
        assert!(int32s(&r).contains(&(16, 1)));
        assert!(int32s(&r).contains(&(13, 0)));
    }

    /// CBUG-B11 — a NONZERO SoftTrigger write latches the trigger and (with
    /// FlushOnSoftTrig > 0) flushes the ring from the write itself
    /// (NDPluginCircularBuff.cpp:271, :276-277), rather than waiting for the next
    /// frame. Writing **0** does neither.
    ///
    /// This test used to be `test_soft_trigger_write_latches_for_any_value_and_flushes`
    /// and pinned C's bug: C stores `value` (:268) and then never tests it, so
    /// `caput SoftTrigger 0` — the natural way to disarm, and what an autosave/PINI
    /// restore writes at boot — armed the plugin and flushed the pre-trigger ring
    /// downstream. Both of the assertions below that now expect "no trigger, no
    /// flush" were written the other way round.
    #[test]
    fn test_soft_trigger_write_latches_only_for_a_nonzero_value() {
        use ad_core_rs::ndarray::{NDDataType, NDDimension};
        use ad_core_rs::plugin::runtime::{
            ParamChangeValue, PluginParamSnapshot, SideParam, WriteVerdict,
        };

        let soft_trigger_write = |p: &mut CircularBuffProcessor, value: i32| {
            let reason = p.params.soft_trigger.unwrap();
            p.on_param_change(
                reason,
                &PluginParamSnapshot {
                    enable_callbacks: true,
                    reason,
                    addr: 0,
                    value: ParamChangeValue::Int32(value),
                },
            )
        };
        let processor = |flush_on_soft_trig: i32| {
            let mut p = CircularBuffProcessor::new(3, 2, TriggerCondition::default(), 100);
            p.buffer().start(); // C: NDCircBuffControl = 1
            p.params.soft_trigger = Some(20);
            p.params.triggered = Some(21);
            p.buffer().set_flush_on_soft_trigger(flush_on_soft_trig);
            let pool = NDArrayPool::new(0);
            // Two frames into the pre-buffer.
            for id in 1..=2 {
                let mut a = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
                a.unique_id = id;
                p.process_array(&Arc::new(a), &pool);
            }
            assert_eq!(p.buffer().pre_buffer_len(), 2);
            p
        };

        // C latches Triggered inside the write (:271), so validate sets it.
        let latched = |p: &CircularBuffProcessor, value: i32| {
            let mut base = asyn_rs::port::PortDriverBase::new(
                "T",
                1,
                Default::default(),
                asyn_rs::port::InterfaceMask::UNMIGRATED_ALL,
                asyn_rs::port::InterfaceMask::UNMIGRATED_INTERRUPTS,
            );
            p.validate_int32(
                20,
                0,
                value,
                WriteVerdict::Accept(value),
                &mut ad_core_rs::plugin::runtime::WritePort::new(&mut base),
            )
            .side
            .contains(&SideParam::Int32 {
                reason: 21,
                addr: 0,
                value: 1,
            })
        };

        // Writing 0 with FlushOnSoftTrig = 1 — the disarm case, and the one C
        // gets wrong: no trigger, no flush, the ring stays intact.
        let mut p = processor(1);
        let r = soft_trigger_write(&mut p, 0);
        assert!(!p.buffer().is_triggered(), "SoftTrigger 0 must not arm");
        assert!(!latched(&p, 0), "SoftTrigger 0 must not post Triggered=1");
        assert!(r.addressed.is_empty(), "SoftTrigger 0 must not flush");
        assert_eq!(p.buffer().pre_buffer_len(), 2);

        // FlushOnSoftTrig = 0: a nonzero value latches, and the ring stays put
        // (it flushes lazily with the first post-trigger frame).
        let mut p = processor(0);
        let r = soft_trigger_write(&mut p, 1);
        assert!(p.buffer().is_triggered());
        assert!(latched(&p, 1));
        assert!(r.addressed.is_empty(), "no flush when FlushOnSoftTrig = 0");
        assert_eq!(p.buffer().pre_buffer_len(), 2);

        // FlushOnSoftTrig = 1: the write itself flushes the pre-buffer, in order.
        let mut p = processor(1);
        let r = soft_trigger_write(&mut p, 1);
        assert!(p.buffer().is_triggered());
        let ids: Vec<_> = r.addressed.iter().map(|(_, a)| a.unique_id).collect();
        assert_eq!(ids, vec![1, 2], "pre-buffer flushed from the write");
        assert_eq!(p.buffer().pre_buffer_len(), 0);
        assert!(latched(&p, 1));

        // The next frame is the first post-trigger frame; the ring is already
        // drained, so C's second flushPreBuffer() (:165) is a no-op and only the
        // new frame is forwarded.
        let pool = NDArrayPool::new(0);
        let mut a = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        a.unique_id = 3;
        let r = p.process_array(&Arc::new(a), &pool);
        let ids: Vec<_> = r.addressed.iter().map(|(_, a)| a.unique_id).collect();
        assert_eq!(ids, vec![3], "pre-buffer already flushed, not re-emitted");
    }

    /// R11-63: the soft-trigger flush is C's `if (flushOn > 0)`
    /// (NDPluginCircularBuff.cpp:276), not `flushOn != 0`. Boundary sweep of the
    /// written FlushOnSoftTrig value through the real param-write path: -1 and 0
    /// must NOT flush (the ring drains lazily on the first post-trigger frame),
    /// 1 must.
    #[test]
    fn r11_63_flush_on_soft_trig_requires_a_positive_value() {
        use ad_core_rs::ndarray::{NDDataType, NDDimension};
        use ad_core_rs::plugin::runtime::{ParamChangeValue, PluginParamSnapshot};

        const FLUSH_ON: usize = 22;
        const SOFT_TRIG: usize = 20;

        let write = |p: &mut CircularBuffProcessor, reason: usize, value: i32| {
            p.on_param_change(
                reason,
                &PluginParamSnapshot {
                    enable_callbacks: true,
                    reason,
                    addr: 0,
                    value: ParamChangeValue::Int32(value),
                },
            )
        };

        for (flush_on, expect_flush) in [(-1, false), (0, false), (1, true)] {
            let mut p = CircularBuffProcessor::new(3, 2, TriggerCondition::default(), 100);
            p.buffer().start();
            p.params.soft_trigger = Some(SOFT_TRIG);
            p.params.triggered = Some(21);
            p.params.flush_on_soft_trigger = Some(FLUSH_ON);

            write(&mut p, FLUSH_ON, flush_on);
            assert_eq!(
                p.buffer().flushes_on_soft_trigger(),
                expect_flush,
                "FlushOnSoftTrig = {flush_on}: C flushes only when > 0"
            );

            let pool = NDArrayPool::new(0);
            for id in 1..=2 {
                let mut a = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
                a.unique_id = id;
                p.process_array(&Arc::new(a), &pool);
            }
            assert_eq!(p.buffer().pre_buffer_len(), 2);

            let r = write(&mut p, SOFT_TRIG, 1);
            assert!(p.buffer().is_triggered());
            if expect_flush {
                let ids: Vec<_> = r.addressed.iter().map(|(_, a)| a.unique_id).collect();
                assert_eq!(ids, vec![1, 2], "FlushOnSoftTrig = {flush_on}: flushed");
                assert_eq!(p.buffer().pre_buffer_len(), 0);
            } else {
                assert!(
                    r.addressed.is_empty(),
                    "FlushOnSoftTrig = {flush_on}: C does not flush from the write"
                );
                assert_eq!(p.buffer().pre_buffer_len(), 2);
            }
        }
    }

    #[test]
    fn test_buffer_status_transitions() {
        let mut cb = CircularBuffer::new(2, 1, TriggerCondition::default());
        cb.set_preset_trigger_count(0); // unlimited triggers

        // Before Control=1 the plugin is idle and records nothing.
        assert_eq!(cb.status(), BufferStatus::Idle);

        // C's Control=1 write posts "Buffer filling" itself
        // (NDPluginCircularBuff.cpp:252-253), before any frame arrives.
        cb.start();
        assert_eq!(cb.status(), BufferStatus::BufferFilling);

        cb.push(make_array(1));
        assert_eq!(cb.status(), BufferStatus::BufferFilling);

        cb.push(make_array(2));
        assert_eq!(cb.status(), BufferStatus::BufferFilling);

        // Trigger -> Flushing
        cb.trigger();
        assert_eq!(cb.status(), BufferStatus::Flushing);

        // Post-trigger capture completes -> back to BufferFilling
        let done = cb.push(make_array(3));
        assert!(done.sequence_done);
        assert_eq!(cb.status(), BufferStatus::BufferFilling);

        // Reset -> Idle
        cb.reset();
        assert_eq!(cb.status(), BufferStatus::Idle);
        assert_eq!(cb.trigger_count(), 0);
    }
}
