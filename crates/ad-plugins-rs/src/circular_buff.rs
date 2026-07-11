use std::collections::VecDeque;
use std::sync::Arc;

use ad_core_rs::ndarray::NDArray;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};
use epics_base_rs::calc;

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
    pub fn evaluate_vars(&self, vars: &[f64; calc::CALC_NARGS]) -> f64 {
        let mut inputs = calc::NumericInputs::with_vars(*vars);
        calc::eval(&self.compiled, &mut inputs).unwrap_or(0.0)
    }
}

/// Trigger condition for circular buffer.
#[derive(Debug, Clone)]
pub enum TriggerCondition {
    /// Trigger on an attribute value exceeding threshold.
    AttributeThreshold { name: String, threshold: f64 },
    /// External trigger (manual).
    External,
    /// Calculated trigger based on attribute values and an expression.
    ///
    /// The C++ calc engine passes: A=attrValueA, B=attrValueB, C=preTrigger,
    /// D=postTrigger, E=currentImage, F=triggered.
    Calc {
        attr_a: String,
        attr_b: String,
        expression: CalcExpression,
    },
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
    /// Result of the trigger calc expression.
    pub calc: f64,
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
    /// `NDCircBuffTriggered` — C `:127`/`:133` (every frame that evaluates the
    /// trigger) and `:192`/`:194` (cleared when the sequence ends).
    pub triggered: Option<i32>,
    /// `NDCircBuffCurrentImage` — C `:151`, the pre-buffer size, assigned ONLY
    /// on a pre-trigger frame.
    pub current_image: Option<i32>,
    /// `NDCircBuffPostCount` — C `:168-169` per forwarded post-trigger frame,
    /// and `:193` (reset to 0) when the sequence re-arms.
    pub post_count: Option<i32>,
    /// `NDCircBuffActualTriggerCount` — C `:179-180`, incremented when the
    /// post-trigger count is reached, not when the trigger fires.
    pub actual_trigger_count: Option<i32>,
    /// `NDCircBuffSoftTrigger` — C `:191`, the soft-trigger latch cleared on
    /// re-arm.
    pub soft_trigger: Option<i32>,
    /// `NDCircBuffControl` — C `:190` (re-arm, still 1) and `:197` (the preset
    /// trigger count was reached: C turns acquisition off).
    pub control: Option<i32>,
    /// `NDCircBuffStatus` — C's `setStringParam` calls (`:152-153`, `:157`,
    /// `:194-195`, `:198`).
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
    pub(crate) pre_count: usize,
    pub(crate) post_count: usize,
    buffer: VecDeque<Arc<NDArray>>,
    pub(crate) trigger_condition: TriggerCondition,
    triggered: bool,
    /// Number of post-trigger frames forwarded so far for the current trigger.
    post_done: usize,
    /// True once the pre-buffer has been flushed for the current trigger.
    pre_flushed: bool,
    /// Frames captured for the current sequence (pre + post), for callers
    /// that want the batch via [`CircularBuffer::take_captured`].
    captured: Vec<Arc<NDArray>>,
    /// Maximum number of triggers before stopping (0 = unlimited).
    preset_trigger_count: usize,
    /// C `actualTriggerCount`: capture sequences *completed* so far — bumped
    /// when the post-trigger count is reached (NDPluginCircularBuff.cpp:179),
    /// not when the trigger fires.
    trigger_count: usize,
    /// If true, flush buffer immediately on soft trigger.
    flush_on_soft_trigger: bool,
    /// Current buffer status.
    pub(crate) status: BufferStatus,
}

impl CircularBuffer {
    pub fn new(pre_count: usize, post_count: usize, condition: TriggerCondition) -> Self {
        Self {
            pre_count,
            post_count,
            buffer: VecDeque::with_capacity(pre_count + 1),
            trigger_condition: condition,
            triggered: false,
            post_done: 0,
            pre_flushed: false,
            captured: Vec::new(),
            preset_trigger_count: 0,
            trigger_count: 0,
            flush_on_soft_trigger: false,
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

    /// Set flush_on_soft_trigger flag.
    pub fn set_flush_on_soft_trigger(&mut self, flush: bool) {
        self.flush_on_soft_trigger = flush;
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

        // If acquisition is completed, ignore new frames
        if self.status == BufferStatus::AcquisitionCompleted {
            return result;
        }

        // Transition from Idle to BufferFilling on first push
        if self.status == BufferStatus::Idle {
            self.status = BufferStatus::BufferFilling;
        }

        if self.triggered {
            // Post-trigger capture (Flushing state). C `:154`.
            result.params.status = Some("Flushing");
            // Flush the pre-buffer once, before the first post-trigger frame.
            if !self.pre_flushed {
                self.pre_flushed = true;
                let pre: Vec<_> = self.buffer.drain(..).collect();
                self.captured.extend(pre.iter().cloned());
                result.forward.extend(pre);
            }
            // C++ increments currentPostCount, posts it, forwards the frame,
            // then tests `currentPostCount >= postCount` (`:168-176`).
            self.captured.push(Arc::clone(&array));
            result.forward.push(array);
            self.post_done += 1;
            result.params.post_count = Some(self.post_done as i32);
            if self.post_done >= self.post_count {
                self.complete_sequence(&mut result);
            }
            return result;
        }

        // Check trigger condition BEFORE adding to pre-buffer,
        // so the triggering frame becomes the first post-trigger frame.
        let trigger = match &self.trigger_condition {
            TriggerCondition::AttributeThreshold { name, threshold } => array
                .attributes
                .get(name)
                .and_then(|a| a.value.as_f64())
                .map(|v| v >= *threshold)
                .unwrap_or(false),
            TriggerCondition::External => false,
            TriggerCondition::Calc {
                attr_a,
                attr_b,
                expression,
            } => {
                let a = array
                    .attributes
                    .get(attr_a)
                    .and_then(|a| a.value.as_f64())
                    .unwrap_or(f64::NAN);
                let b = array
                    .attributes
                    .get(attr_b)
                    .and_then(|a| a.value.as_f64())
                    .unwrap_or(f64::NAN);
                // C++ passes: A=attrValueA, B=attrValueB, C=preTrigger,
                // D=postTrigger, E=currentImage, F=triggered
                let mut vars = [0.0f64; calc::CALC_NARGS];
                vars[0] = a; // A
                vars[1] = b; // B
                vars[2] = self.pre_count as f64; // C
                vars[3] = self.post_count as f64; // D
                vars[4] = self.buffer.len() as f64; // E (currentImage)
                vars[5] = if self.triggered { 1.0 } else { 0.0 }; // F
                let calc = expression.evaluate_vars(&vars);
                // C posts TriggerAVal/BVal/CalcVal every evaluated frame
                // (NDPluginCircularBuff.cpp:67-78), regardless of the outcome.
                result.trigger_values = Some(TriggerValues { a, b, calc });
                // C fires only when the result is a finite non-zero
                // (NDPluginCircularBuff.cpp:77 `!isnan && !isinf && != 0`); a
                // NaN/Inf result (e.g. a missing trigger attribute → epicsNAN,
                // or an `A/B` with a zero denominator) must NOT trigger.
                // `f64::is_finite` is exactly `!isnan && !isinf`.
                calc.is_finite() && calc != 0.0
            }
        };

        // C `:133` posts the trigger flag on every frame that evaluated the
        // trigger calc, whether it fired or not.
        result.params.triggered = Some(i32::from(trigger));

        if trigger {
            // Trigger fires before adding this frame to the pre-buffer,
            // so the triggering frame will be the first post-trigger frame.
            self.trigger();
            result.params.status = Some("Flushing");
            // Flush the pre-buffer immediately, then forward the triggering
            // frame as the first post-trigger frame (C++ flushPreBuffer +
            // doCallbacksGenericPointer of the trigger frame).
            self.pre_flushed = true;
            let pre: Vec<_> = self.buffer.drain(..).collect();
            self.captured.extend(pre.iter().cloned());
            result.forward.extend(pre);
            self.captured.push(Arc::clone(&array));
            result.forward.push(array);
            self.post_done += 1;
            result.params.post_count = Some(self.post_done as i32);
            if self.post_done >= self.post_count {
                self.complete_sequence(&mut result);
            }
            return result;
        }

        // Maintain pre-trigger ring buffer
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

        result
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
            // C `:194-198`: preset reached — clear the trigger and turn
            // acquisition off (NDCircBuffControl = 0).
            self.status = BufferStatus::AcquisitionCompleted;
            result.params.triggered = Some(0);
            result.params.control = Some(0);
            result.params.status = Some("Acquisition Completed");
        } else {
            // C `:188-195`: re-arm for the next trigger — the soft-trigger
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
        // Don't trigger if acquisition already completed
        if self.status == BufferStatus::AcquisitionCompleted {
            return;
        }

        self.triggered = true;
        self.post_done = 0;
        self.pre_flushed = false;
        self.status = BufferStatus::Flushing;
        // The pre-buffer is flushed lazily on the first post-trigger push so
        // the frames stream out in order with the post-trigger frames.
        self.captured.clear();
    }

    /// Take the captured arrays (pre + post trigger).
    pub fn take_captured(&mut self) -> Vec<Arc<NDArray>> {
        std::mem::take(&mut self.captured)
    }

    pub fn is_triggered(&self) -> bool {
        self.triggered
    }

    pub fn pre_buffer_len(&self) -> usize {
        self.buffer.len()
    }

    pub fn reset(&mut self) {
        self.buffer.clear();
        self.captured.clear();
        self.triggered = false;
        self.post_done = 0;
        self.pre_flushed = false;
        self.trigger_count = 0;
        self.status = BufferStatus::Idle;
    }
}

// --- New CircularBuffProcessor (NDPluginProcess-based) ---

/// CircularBuff processor: maintains ring buffer state, emits captured arrays on trigger.
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

pub struct CircularBuffProcessor {
    buffer: CircularBuffer,
    params: CBParamIndices,
    /// C `maxBuffers_` — the plugin's input NDArray queue size, passed to
    /// `NDCircularBuffConfigure` as `queueSize`. Bounds the accepted pre-count:
    /// C rejects `preCount > maxBuffers_ - 1` (NDPluginCircularBuff.cpp:284).
    max_buffers: usize,
    // cached trigger attribute names and calc expression
    trigger_a_name: String,
    trigger_b_name: String,
    trigger_calc_expr: String,
}

impl CircularBuffProcessor {
    pub fn new(
        pre_count: usize,
        post_count: usize,
        condition: TriggerCondition,
        max_buffers: usize,
    ) -> Self {
        Self {
            buffer: CircularBuffer::new(pre_count, post_count, condition),
            params: CBParamIndices::default(),
            max_buffers,
            trigger_a_name: String::new(),
            trigger_b_name: String::new(),
            trigger_calc_expr: String::new(),
        }
    }

    pub fn trigger(&mut self) {
        self.buffer.trigger();
    }

    pub fn buffer(&self) -> &CircularBuffer {
        &self.buffer
    }

    /// Rebuild the trigger condition from cached attribute names and calc expression.
    fn rebuild_trigger_condition(&mut self) {
        if !self.trigger_calc_expr.is_empty() {
            if let Some(expr) = CalcExpression::parse(&self.trigger_calc_expr) {
                self.buffer.trigger_condition = TriggerCondition::Calc {
                    attr_a: self.trigger_a_name.clone(),
                    attr_b: self.trigger_b_name.clone(),
                    expression: expr,
                };
                return;
            }
        }
        if !self.trigger_a_name.is_empty() {
            self.buffer.trigger_condition = TriggerCondition::AttributeThreshold {
                name: self.trigger_a_name.clone(),
                threshold: 0.5,
            };
        } else {
            self.buffer.trigger_condition = TriggerCondition::External;
        }
    }
}

impl NDPluginProcess for CircularBuffProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        use ad_core_rs::plugin::runtime::ParamUpdate;

        let push_result = self.buffer.push(Arc::new(array.clone()));

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
            if let Some(idx) = self.params.trigger_calc_val {
                updates.push(ParamUpdate::float64(idx, tv.calc));
            }
        }

        // Stream frames downstream as the C++ plugin does: pre-buffer frames
        // are flushed at the trigger and each post-trigger frame is forwarded
        // immediately, rather than being withheld until the sequence ends.
        if push_result.forward.is_empty() {
            ProcessResult::sink(updates)
        } else {
            let mut result = ProcessResult::arrays(push_result.forward);
            result.param_updates = updates;
            result
        }
    }

    fn plugin_type(&self) -> &str {
        "NDPluginCircularBuff"
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

        // C sets NDCircBuffStatus to "Idle" in the constructor
        // (NDPluginCircularBuff.cpp:432).
        if let Some(idx) = self.params.status {
            base.set_string_param(idx, 0, "Idle".into())?;
        }
        Ok(())
    }

    fn on_param_change(
        &mut self,
        reason: usize,
        params: &ad_core_rs::plugin::runtime::PluginParamSnapshot,
    ) -> ad_core_rs::plugin::runtime::ParamChangeResult {
        use ad_core_rs::plugin::runtime::{ParamChangeResult, ParamChangeValue, ParamUpdate};

        let mut updates = Vec::new();
        if Some(reason) == self.params.control {
            let v = params.value.as_i32();
            if v == 1 {
                // Start. C writeInt32(Control=1) rebuilds the ring and zeroes
                // the whole runtime counter set before posting the status
                // (NDPluginCircularBuff.cpp:249-254).
                self.buffer.reset();
                self.buffer.status = BufferStatus::BufferFilling;
                for (index, value) in [
                    (self.params.soft_trigger, 0),
                    (self.params.triggered, 0),
                    (self.params.post_count, 0),
                    (self.params.actual_trigger_count, 0),
                ] {
                    if let Some(idx) = index {
                        updates.push(ParamUpdate::int32(idx, value));
                    }
                }
                // C writeInt32(Control=1): "Buffer filling"/"Dropping frames"
                // (NDPluginCircularBuff.cpp:255).
                if let Some(idx) = self.params.status {
                    let s = if self.buffer.pre_count > 0 {
                        "Buffer filling"
                    } else {
                        "Dropping frames"
                    };
                    updates.push(ParamUpdate::octet(idx, s.to_string()));
                }
            } else {
                // Stop. C writeInt32(Control=0) clears the trigger latches and
                // the displayed image count (NDPluginCircularBuff.cpp:257-259).
                self.buffer.status = BufferStatus::Idle;
                for (index, value) in [
                    (self.params.soft_trigger, 0),
                    (self.params.triggered, 0),
                    (self.params.current_image, 0),
                ] {
                    if let Some(idx) = index {
                        updates.push(ParamUpdate::int32(idx, value));
                    }
                }
                // C writeInt32(Control=0): "Acquisition Stopped"
                // (NDPluginCircularBuff.cpp:260).
                if let Some(idx) = self.params.status {
                    updates.push(ParamUpdate::octet(idx, "Acquisition Stopped".to_string()));
                }
            }
        } else if Some(reason) == self.params.pre_trigger {
            // C writeInt32(NDCircBuffPreTrigger) validates before committing
            // (NDPluginCircularBuff.cpp:280-292), in this exact order: reject
            // while running, then a pre-count above `maxBuffers_-1`, then a
            // negative value (each leaves the param at its old value with an
            // explanatory status string), otherwise commit.
            let value = params.value.as_i32();
            let running = matches!(
                self.buffer.status(),
                BufferStatus::BufferFilling | BufferStatus::Flushing
            );
            let reject_msg = if running {
                Some("Stop acquisition to set pre-count")
            } else if value > self.max_buffers as i32 - 1 {
                // The pre-trigger ring cannot exceed the input queue (C 284).
                Some("Pre-count too high")
            } else if value < 0 {
                Some("Invalid pre-count value")
            } else {
                None
            };
            if let Some(msg) = reject_msg {
                if let Some(idx) = self.params.status {
                    updates.push(ParamUpdate::octet(idx, msg.to_string()));
                }
                // Revert the pre-committed param to the last accepted value
                // (C never calls setIntegerParam on the reject paths).
                if let Some(idx) = self.params.pre_trigger {
                    updates.push(ParamUpdate::int32(idx, self.buffer.pre_count as i32));
                }
            } else {
                self.buffer.pre_count = value as usize;
            }
        } else if Some(reason) == self.params.post_trigger {
            self.buffer.post_count = params.value.as_i32().max(0) as usize;
        } else if Some(reason) == self.params.preset_trigger_count {
            self.buffer
                .set_preset_trigger_count(params.value.as_i32().max(0) as usize);
        } else if Some(reason) == self.params.flush_on_soft_trigger {
            self.buffer
                .set_flush_on_soft_trigger(params.value.as_i32() != 0);
        } else if Some(reason) == self.params.soft_trigger {
            if params.value.as_i32() != 0 {
                self.buffer.trigger();
                // C writeInt32(SoftTrigger) posts the trigger flag straight from
                // the write (NDPluginCircularBuff.cpp:270) — the flushing frames
                // that follow never touch it again.
                if let Some(idx) = self.params.triggered {
                    updates.push(ParamUpdate::int32(idx, 1));
                }
            }
        } else if Some(reason) == self.params.trigger_a {
            if let ParamChangeValue::Octet(s) = &params.value {
                self.trigger_a_name = s.clone();
                self.rebuild_trigger_condition();
            }
        } else if Some(reason) == self.params.trigger_b {
            if let ParamChangeValue::Octet(s) = &params.value {
                self.trigger_b_name = s.clone();
                self.rebuild_trigger_condition();
            }
        } else if Some(reason) == self.params.trigger_calc {
            if let ParamChangeValue::Octet(s) = &params.value {
                self.trigger_calc_expr = s.clone();
                self.rebuild_trigger_condition();
            }
        }

        ParamChangeResult::updates(updates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let mut cb = CircularBuffer::new(3, 2, TriggerCondition::External);

        for i in 0..5 {
            cb.push(make_array(i));
        }
        // Pre-buffer should hold last 3
        assert_eq!(cb.pre_buffer_len(), 3);
    }

    #[test]
    fn test_external_trigger() {
        let mut cb = CircularBuffer::new(2, 2, TriggerCondition::External);

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

        let captured = cb.take_captured();
        assert_eq!(captured.len(), 4); // 2 pre + 2 post
        assert_eq!(captured[0].unique_id, 2);
        assert_eq!(captured[1].unique_id, 3);
        assert_eq!(captured[2].unique_id, 4);
        assert_eq!(captured[3].unique_id, 5);
    }

    #[test]
    fn test_post_count_zero_no_underflow() {
        // Regression: post_count == 0 must complete the sequence on the first
        // post-trigger frame instead of underflowing the post counter.
        let mut cb = CircularBuffer::new(2, 0, TriggerCondition::External);
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
        let r2 = cb.push(make_array(4));
        assert!(!r2.sequence_done);
        assert!(r2.forward.is_empty());
    }

    #[test]
    fn test_attribute_trigger_post_count_zero() {
        // post_count == 0 with an attribute trigger: the triggering frame is
        // forwarded and the sequence completes on the same push.
        let mut cb = CircularBuffer::new(
            1,
            0,
            TriggerCondition::AttributeThreshold {
                name: "trigger".into(),
                threshold: 5.0,
            },
        );
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
            TriggerCondition::AttributeThreshold {
                name: "trigger".into(),
                threshold: 5.0,
            },
        );

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

        let captured = cb.take_captured();
        // 1 pre (id=2) + 2 post (id=3 triggering frame + id=4)
        assert_eq!(captured.len(), 3);
        assert_eq!(captured[0].unique_id, 2);
        assert_eq!(captured[1].unique_id, 3);
        assert_eq!(captured[2].unique_id, 4);
    }

    // --- New tests ---

    #[test]
    fn test_calc_trigger() {
        // Expression: "A>5" — trigger when attribute A exceeds 5
        let expr = CalcExpression::parse("A>5").unwrap();
        let mut cb = CircularBuffer::new(
            1,
            2,
            TriggerCondition::Calc {
                attr_a: "attr_a".into(),
                attr_b: "attr_b".into(),
                expression: expr,
            },
        );

        // A=3, should not trigger
        cb.push(make_array_with_attrs(1, 3.0, 0.0));
        assert!(!cb.is_triggered());

        // A=6, should trigger; triggering frame is first post-trigger
        cb.push(make_array_with_attrs(2, 6.0, 0.0));
        assert!(cb.is_triggered());

        let done = cb.push(make_array(3));
        assert!(done.sequence_done);

        let captured = cb.take_captured();
        // 1 pre (id=1) + 2 post (id=2 triggering frame + id=3)
        assert_eq!(captured.len(), 3);
        assert_eq!(captured[0].unique_id, 1);
        assert_eq!(captured[1].unique_id, 2);
        assert_eq!(captured[2].unique_id, 3);
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
            TriggerCondition::Calc {
                attr_a: "attr_a".into(),
                attr_b: "attr_b".into(),
                expression: expr,
            },
        );

        // Frame with A=3, B=4 → calc=7 (nonzero → triggers).
        let r = cb.push(make_array_with_attrs(1, 3.0, 4.0));
        let tv = r.trigger_values.expect("calc path surfaces trigger values");
        assert_eq!(tv.a, 3.0);
        assert_eq!(tv.b, 4.0);
        assert_eq!(tv.calc, 7.0);

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
            TriggerCondition::Calc {
                attr_a: "missing_a".into(),
                attr_b: "missing_b".into(),
                expression: expr,
            },
        );
        let r = cb.push(make_array(1));
        let tv = r.trigger_values.expect("calc path surfaces trigger values");
        assert!(tv.a.is_nan());
        assert!(tv.b.is_nan());
        assert!(tv.calc.is_nan());
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
                TriggerCondition::Calc {
                    attr_a: "attr_a".into(),
                    attr_b: "attr_b".into(),
                    expression: expr,
                },
            );
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
        assert_eq!(expr.evaluate_vars(&vars), 1.0);
        vars[2] = 0.0; // C=0 should fail the condition
        assert_eq!(expr.evaluate_vars(&vars), 0.0);

        // Invalid expression returns None
        assert!(CalcExpression::parse("@@@").is_none());
    }

    #[test]
    fn test_preset_trigger_count() {
        let mut cb = CircularBuffer::new(1, 1, TriggerCondition::External);
        cb.set_preset_trigger_count(2);

        assert_eq!(cb.status(), BufferStatus::Idle);

        // First push transitions to BufferFilling
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

        cb.take_captured();

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

        cb.take_captured();

        // Further frames should be ignored
        let done = cb.push(make_array(5));
        assert!(!done.sequence_done);
        assert_eq!(cb.status(), BufferStatus::AcquisitionCompleted);

        // Further triggers should be ignored
        cb.trigger();
        assert_eq!(cb.trigger_count(), 2); // unchanged
    }

    #[test]
    fn test_stop_resets_current_image_and_status() {
        // Regression for ADP-43 (+ ADP-40 stop string): a Control=0 write posts
        // CURRENT_IMAGE=0 and STATUS="Acquisition Stopped".
        use ad_core_rs::plugin::runtime::{ParamChangeValue, ParamUpdate, PluginParamSnapshot};

        let mut processor = CircularBuffProcessor::new(2, 1, TriggerCondition::External, 100);
        processor.params.control = Some(10);
        processor.params.current_image = Some(11);
        processor.params.status = Some(12);

        let snapshot = PluginParamSnapshot {
            enable_callbacks: true,
            reason: 10,
            addr: 0,
            value: ParamChangeValue::Int32(0), // stop
        };
        let result = processor.on_param_change(10, &snapshot);

        assert!(
            result.param_updates.iter().any(|u| matches!(
                u,
                ParamUpdate::Int32 {
                    reason: 11,
                    value: 0,
                    ..
                }
            )),
            "stop must post CURRENT_IMAGE=0"
        );
        assert!(
            result.param_updates.iter().any(|u| matches!(
                u,
                ParamUpdate::Octet { reason: 12, value, .. } if value == "Acquisition Stopped"
            )),
            "stop must post STATUS=Acquisition Stopped"
        );
    }

    #[test]
    fn test_pre_count_validation() {
        // Regression for ADP-44: pre-count writes are rejected (status string +
        // param reverted) while running, above the maxBuffers-1 ceiling, and
        // for negative values, accepted otherwise (C NDPluginCircularBuff.cpp:
        // 280-292). maxBuffers_ is 10 here, so the ceiling is 9.
        use ad_core_rs::plugin::runtime::{ParamChangeValue, ParamUpdate, PluginParamSnapshot};

        let make_proc = || {
            let mut p = CircularBuffProcessor::new(3, 1, TriggerCondition::External, 10);
            p.params.pre_trigger = Some(20);
            p.params.status = Some(12);
            p
        };
        let write = |p: &mut CircularBuffProcessor, v: i32| {
            let snap = PluginParamSnapshot {
                enable_callbacks: true,
                reason: 20,
                addr: 0,
                value: ParamChangeValue::Int32(v),
            };
            p.on_param_change(20, &snap)
        };

        // Running → reject, status string, param reverted to old (3), unchanged.
        let mut p = make_proc();
        p.buffer.status = BufferStatus::BufferFilling;
        let r = write(&mut p, 7);
        assert_eq!(
            p.buffer.pre_count, 3,
            "reject while running, value unchanged"
        );
        assert!(r.param_updates.iter().any(|u| matches!(
            u,
            ParamUpdate::Octet { reason: 12, value, .. } if value == "Stop acquisition to set pre-count"
        )));
        assert!(r.param_updates.iter().any(|u| matches!(
            u,
            ParamUpdate::Int32 {
                reason: 20,
                value: 3,
                ..
            }
        )));

        // Stopped + negative → reject with "Invalid pre-count value".
        let mut p = make_proc();
        p.buffer.status = BufferStatus::Idle;
        let r = write(&mut p, -1);
        assert_eq!(p.buffer.pre_count, 3, "negative rejected, value unchanged");
        assert!(r.param_updates.iter().any(|u| matches!(
            u,
            ParamUpdate::Octet { reason: 12, value, .. } if value == "Invalid pre-count value"
        )));

        // Stopped + above maxBuffers-1 (9) → reject with "Pre-count too high".
        let mut p = make_proc();
        p.buffer.status = BufferStatus::Idle;
        let r = write(&mut p, 10);
        assert_eq!(p.buffer.pre_count, 3, "too-high rejected, value unchanged");
        assert!(r.param_updates.iter().any(|u| matches!(
            u,
            ParamUpdate::Octet { reason: 12, value, .. } if value == "Pre-count too high"
        )));
        assert!(r.param_updates.iter().any(|u| matches!(
            u,
            ParamUpdate::Int32 {
                reason: 20,
                value: 3,
                ..
            }
        )));

        // Stopped + exactly maxBuffers-1 (9) → accept (boundary).
        let mut p = make_proc();
        p.buffer.status = BufferStatus::Idle;
        write(&mut p, 9);
        assert_eq!(p.buffer.pre_count, 9, "valid pre-count committed");
    }

    #[test]
    fn test_frame_status_strings() {
        // ADP-40: NDCircBuffStatus is an Octet string and `push` now owns it —
        // the exact C strings, on exactly the frames C calls setStringParam.
        // Filling below capacity: C makes no setStringParam call at all.
        let mut cb = CircularBuffer::new(2, 2, TriggerCondition::External);
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
        let mut cb = CircularBuffer::new(0, 1, TriggerCondition::External);
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
        let mut cb = CircularBuffer::new(2, 1, TriggerCondition::External);
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
        // then resets it to 0 when the sequence re-arms (:193). The port cached
        // the param index but never emitted an update, so PostCount_RBV read 0
        // forever.
        let mut cb = CircularBuffer::new(2, 3, TriggerCondition::External);
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
        // (:194-198 has no setIntegerParam(NDCircBuffPostCount, 0)), so the
        // final count stays visible after the preset trigger count is reached.
        let mut cb = CircularBuffer::new(1, 2, TriggerCondition::External);
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
        let mut cb = CircularBuffer::new(3, 2, TriggerCondition::External);
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
        let mut cb = CircularBuffer::new(1, 2, TriggerCondition::External);
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

    #[test]
    fn test_processor_emits_the_frame_params() {
        // The processor maps `FrameParams` onto the registered indices: a
        // flushing frame must emit POST_COUNT and leave CURRENT_IMAGE alone.
        use ad_core_rs::ndarray::{NDDataType, NDDimension};
        use ad_core_rs::plugin::runtime::ParamUpdate;

        let mut p = CircularBuffProcessor::new(2, 2, TriggerCondition::External, 100);
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
        let r = p.process_array(&frame(), &pool);
        assert!(int32s(&r).contains(&(11, 1)));
        assert!(!int32s(&r).iter().any(|(reason, _)| *reason == 13));

        // Flushing frame: POST_COUNT=1 and NO CURRENT_IMAGE update (the pre-fix
        // processor posted CURRENT_IMAGE=0 here and never posted POST_COUNT).
        p.trigger();
        let r = p.process_array(&frame(), &pool);
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
        let r = p.process_array(&frame(), &pool);
        assert!(int32s(&r).contains(&(16, 1)));
        assert!(int32s(&r).contains(&(13, 0)));
    }

    #[test]
    fn test_buffer_status_transitions() {
        let mut cb = CircularBuffer::new(2, 1, TriggerCondition::External);

        // Initial state
        assert_eq!(cb.status(), BufferStatus::Idle);

        // First push -> BufferFilling
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
