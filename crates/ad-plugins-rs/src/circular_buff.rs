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

/// Result of pushing a frame: which frames to forward downstream now, and
/// whether a capture sequence completed on this push.
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
    /// Number of triggers fired so far.
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

    /// Get the current trigger count.
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
            // Post-trigger capture (Flushing state).
            // Flush the pre-buffer once, before the first post-trigger frame.
            if !self.pre_flushed {
                self.pre_flushed = true;
                let pre: Vec<_> = self.buffer.drain(..).collect();
                self.captured.extend(pre.iter().cloned());
                result.forward.extend(pre);
            }
            // C++ increments currentPostCount, forwards the frame, then tests
            // `currentPostCount >= postCount`.
            self.captured.push(Arc::clone(&array));
            result.forward.push(array);
            self.post_done += 1;
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
                calc != 0.0
            }
        };

        if trigger {
            // Trigger fires before adding this frame to the pre-buffer,
            // so the triggering frame will be the first post-trigger frame.
            self.trigger();
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

        result
    }

    /// Finalize a completed post-trigger sequence (C++
    /// `currentPostCount >= postCount` branch): advance status / trigger
    /// bookkeeping and signal completion.
    fn complete_sequence(&mut self, result: &mut PushResult) {
        self.triggered = false;
        self.pre_flushed = false;
        self.post_done = 0;
        if self.preset_trigger_count > 0 && self.trigger_count >= self.preset_trigger_count {
            self.status = BufferStatus::AcquisitionCompleted;
        } else {
            self.status = BufferStatus::BufferFilling;
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
        self.trigger_count += 1;
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

/// The status string C `NDPluginCircularBuff::processCallbacks` leaves in
/// `NDCircBuffStatus` (asynOctet) after one frame, given the post-push state.
///
/// Mirrors NDPluginCircularBuff.cpp:153-195: on a flushing frame the status is
/// "Flushing"; once the post-trigger count is reached it becomes
/// "Buffer filling"/"Dropping frames" (more triggers allowed) or
/// "Acquisition Completed" (preset reached); while filling it only changes when
/// the pre-buffer reaches its capacity ("Buffer Wrapping"/"Dropping frames").
/// `None` means C makes no `setStringParam` call this frame (status unchanged).
fn flush_path_status(
    sequence_done: bool,
    acquisition_completed: bool,
    forwarded: bool,
    pre_buffer_full: bool,
    pre_count: usize,
) -> Option<&'static str> {
    if sequence_done {
        Some(if acquisition_completed {
            "Acquisition Completed"
        } else if pre_count > 0 {
            "Buffer filling"
        } else {
            "Dropping frames"
        })
    } else if forwarded {
        Some("Flushing")
    } else if pre_buffer_full {
        Some(if pre_count > 0 {
            "Buffer Wrapping"
        } else {
            "Dropping frames"
        })
    } else {
        None
    }
}

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
    // cached trigger attribute names and calc expression
    trigger_a_name: String,
    trigger_b_name: String,
    trigger_calc_expr: String,
}

impl CircularBuffProcessor {
    pub fn new(pre_count: usize, post_count: usize, condition: TriggerCondition) -> Self {
        Self {
            buffer: CircularBuffer::new(pre_count, post_count, condition),
            params: CBParamIndices::default(),
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

        let mut updates = Vec::new();
        if let Some(idx) = self.params.status {
            // C NDCircBuffStatus is asynOctet: emit the status string C leaves
            // after this frame (NDPluginCircularBuff.cpp:153-195), or nothing
            // when C makes no setStringParam call (still filling below capacity).
            let pre_count = self.buffer.pre_count;
            if let Some(s) = flush_path_status(
                push_result.sequence_done,
                self.buffer.status() == BufferStatus::AcquisitionCompleted,
                !push_result.forward.is_empty(),
                self.buffer.pre_buffer_len() == pre_count,
                pre_count,
            ) {
                updates.push(ParamUpdate::octet(idx, s.to_string()));
            }
        }
        if let Some(idx) = self.params.current_image {
            updates.push(ParamUpdate::int32(idx, self.buffer.pre_buffer_len() as i32));
        }
        if let Some(idx) = self.params.triggered {
            updates.push(ParamUpdate::int32(
                idx,
                if self.buffer.is_triggered() { 1 } else { 0 },
            ));
        }
        if let Some(idx) = self.params.actual_trigger_count {
            updates.push(ParamUpdate::int32(idx, self.buffer.trigger_count() as i32));
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
                // Start
                self.buffer.reset();
                self.buffer.status = BufferStatus::BufferFilling;
                // C writeInt32(Control=1): "Buffer filling"/"Dropping frames"
                // (NDPluginCircularBuff.cpp:252-253).
                if let Some(idx) = self.params.status {
                    let s = if self.buffer.pre_count > 0 {
                        "Buffer filling"
                    } else {
                        "Dropping frames"
                    };
                    updates.push(ParamUpdate::octet(idx, s.to_string()));
                }
            } else {
                // Stop
                self.buffer.status = BufferStatus::Idle;
                // C writeInt32(Control=0) resets the displayed image count to 0
                // (NDPluginCircularBuff.cpp:259).
                if let Some(idx) = self.params.current_image {
                    updates.push(ParamUpdate::int32(idx, 0));
                }
                // C writeInt32(Control=0): "Acquisition Stopped"
                // (NDPluginCircularBuff.cpp:260).
                if let Some(idx) = self.params.status {
                    updates.push(ParamUpdate::octet(idx, "Acquisition Stopped".to_string()));
                }
            }
        } else if Some(reason) == self.params.pre_trigger {
            // C writeInt32(NDCircBuffPreTrigger) validates before committing
            // (NDPluginCircularBuff.cpp:280-292): reject while running or for a
            // negative value (status string + leave the param at its old value),
            // otherwise commit. The "Pre-count too high" (> maxBuffers_-1) check
            // is not modelled — the Rust processor carries no maxBuffers bound.
            let value = params.value.as_i32();
            let running = matches!(
                self.buffer.status(),
                BufferStatus::BufferFilling | BufferStatus::Flushing
            );
            let reject_msg = if running {
                Some("Stop acquisition to set pre-count")
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

        // First trigger
        cb.trigger();
        assert_eq!(cb.trigger_count(), 1);
        assert_eq!(cb.status(), BufferStatus::Flushing);

        let done = cb.push(make_array(2));
        assert!(done.sequence_done);
        assert_eq!(cb.status(), BufferStatus::BufferFilling); // back to filling after first capture

        cb.take_captured();

        // Refill buffer
        cb.push(make_array(3));

        // Second trigger — should reach preset count
        cb.trigger();
        assert_eq!(cb.trigger_count(), 2);
        assert_eq!(cb.status(), BufferStatus::Flushing);

        let done = cb.push(make_array(4));
        assert!(done.sequence_done);
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

        let mut processor = CircularBuffProcessor::new(2, 1, TriggerCondition::External);
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
        // param reverted) while running and for negative values, accepted
        // otherwise.
        use ad_core_rs::plugin::runtime::{ParamChangeValue, ParamUpdate, PluginParamSnapshot};

        let make_proc = || {
            let mut p = CircularBuffProcessor::new(3, 1, TriggerCondition::External);
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

        // Stopped + valid → accept.
        let mut p = make_proc();
        p.buffer.status = BufferStatus::Idle;
        write(&mut p, 9);
        assert_eq!(p.buffer.pre_count, 9, "valid pre-count committed");
    }

    #[test]
    fn test_flush_path_status_strings() {
        // Regression for ADP-40: NDCircBuffStatus is an Octet string; the
        // processCallbacks path must leave the exact C strings.
        // Flushing frame (a frame was forwarded, sequence not done).
        assert_eq!(
            flush_path_status(false, false, true, false, 5),
            Some("Flushing")
        );
        // Sequence completes, more triggers allowed, preCount>0.
        assert_eq!(
            flush_path_status(true, false, true, false, 5),
            Some("Buffer filling")
        );
        // Sequence completes, more triggers allowed, preCount==0.
        assert_eq!(
            flush_path_status(true, false, true, false, 0),
            Some("Dropping frames")
        );
        // Sequence completes, preset trigger count reached.
        assert_eq!(
            flush_path_status(true, true, true, false, 5),
            Some("Acquisition Completed")
        );
        // Filling, pre-buffer just reached capacity, preCount>0.
        assert_eq!(
            flush_path_status(false, false, false, true, 5),
            Some("Buffer Wrapping")
        );
        // Filling at capacity with preCount==0 → frames are dropped.
        assert_eq!(
            flush_path_status(false, false, false, true, 0),
            Some("Dropping frames")
        );
        // Still filling below capacity → C makes no setStringParam call.
        assert_eq!(flush_path_status(false, false, false, false, 5), None);
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
