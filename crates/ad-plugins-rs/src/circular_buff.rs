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

/// Circular buffer state for pre/post-trigger capture.
pub struct CircularBuffer {
    pub(crate) pre_count: usize,
    pub(crate) post_count: usize,
    buffer: VecDeque<Arc<NDArray>>,
    pub(crate) trigger_condition: TriggerCondition,
    triggered: bool,
    post_remaining: usize,
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
            post_remaining: 0,
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
    /// Returns true if a complete capture sequence is ready.
    pub fn push(&mut self, array: Arc<NDArray>) -> bool {
        // If acquisition is completed, ignore new frames
        if self.status == BufferStatus::AcquisitionCompleted {
            return false;
        }

        // Transition from Idle to BufferFilling on first push
        if self.status == BufferStatus::Idle {
            self.status = BufferStatus::BufferFilling;
        }

        if self.triggered {
            // Post-trigger capture (Flushing state)
            self.captured.push(array);
            self.post_remaining -= 1;
            if self.post_remaining == 0 {
                self.triggered = false;
                // Check if we've reached the preset trigger count
                if self.preset_trigger_count > 0 && self.trigger_count >= self.preset_trigger_count
                {
                    self.status = BufferStatus::AcquisitionCompleted;
                } else {
                    self.status = BufferStatus::BufferFilling;
                }
                return true;
            }
            return false;
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
                expression.evaluate_vars(&vars) != 0.0
            }
        };

        if trigger {
            // Trigger fires before adding this frame to the pre-buffer,
            // so the triggering frame will be the first post-trigger frame.
            self.trigger();
            // The triggering frame is the first post-trigger capture.
            self.captured.push(array);
            self.post_remaining -= 1;
            if self.post_remaining == 0 {
                self.triggered = false;
                if self.preset_trigger_count > 0 && self.trigger_count >= self.preset_trigger_count
                {
                    self.status = BufferStatus::AcquisitionCompleted;
                } else {
                    self.status = BufferStatus::BufferFilling;
                }
                return true;
            }
            return false;
        }

        // Maintain pre-trigger ring buffer
        self.buffer.push_back(array);
        if self.buffer.len() > self.pre_count {
            self.buffer.pop_front();
        }

        false
    }

    /// External trigger.
    pub fn trigger(&mut self) {
        // Don't trigger if acquisition already completed
        if self.status == BufferStatus::AcquisitionCompleted {
            return;
        }

        self.triggered = true;
        self.post_remaining = self.post_count;
        self.trigger_count += 1;
        self.status = BufferStatus::Flushing;
        // Flush pre-trigger buffer to captured
        self.captured.clear();
        self.captured.extend(self.buffer.drain(..));
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
        self.post_remaining = 0;
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

        let done = self.buffer.push(Arc::new(array.clone()));

        let mut updates = Vec::new();
        if let Some(idx) = self.params.status {
            let status_val = match self.buffer.status() {
                BufferStatus::Idle => 0,
                BufferStatus::BufferFilling => 1,
                BufferStatus::Flushing => 2,
                BufferStatus::AcquisitionCompleted => 3,
            };
            updates.push(ParamUpdate::int32(idx, status_val));
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

        if done {
            let mut result = ProcessResult::arrays(self.buffer.take_captured());
            result.param_updates = updates;
            result
        } else {
            ProcessResult::sink(updates)
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
        base.create_param("CIRC_BUFF_STATUS", ParamType::Int32)?;
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
        Ok(())
    }

    fn on_param_change(
        &mut self,
        reason: usize,
        params: &ad_core_rs::plugin::runtime::PluginParamSnapshot,
    ) -> ad_core_rs::plugin::runtime::ParamChangeResult {
        use ad_core_rs::plugin::runtime::{ParamChangeResult, ParamChangeValue};

        if Some(reason) == self.params.control {
            let v = params.value.as_i32();
            if v == 1 {
                // Start
                self.buffer.reset();
                self.buffer.status = BufferStatus::BufferFilling;
            } else {
                // Stop
                self.buffer.status = BufferStatus::Idle;
            }
        } else if Some(reason) == self.params.pre_trigger {
            self.buffer.pre_count = params.value.as_i32().max(0) as usize;
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

        ParamChangeResult::updates(vec![])
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
        arr.attributes.add(NDAttribute {
            name: "trigger".into(),
            description: "".into(),
            source: NDAttrSource::Driver,
            value: NDAttrValue::Float64(attr_val),
        });
        Arc::new(arr)
    }

    fn make_array_with_attrs(id: i32, a_val: f64, b_val: f64) -> Arc<NDArray> {
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = id;
        arr.attributes.add(NDAttribute {
            name: "attr_a".into(),
            description: "".into(),
            source: NDAttrSource::Driver,
            value: NDAttrValue::Float64(a_val),
        });
        arr.attributes.add(NDAttribute {
            name: "attr_b".into(),
            description: "".into(),
            source: NDAttrSource::Driver,
            value: NDAttrValue::Float64(b_val),
        });
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

        cb.push(make_array(4));
        let done = cb.push(make_array(5));
        assert!(done);

        let captured = cb.take_captured();
        assert_eq!(captured.len(), 4); // 2 pre + 2 post
        assert_eq!(captured[0].unique_id, 2);
        assert_eq!(captured[1].unique_id, 3);
        assert_eq!(captured[2].unique_id, 4);
        assert_eq!(captured[3].unique_id, 5);
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
        cb.push(make_array_with_attr(3, 5.0));
        assert!(cb.is_triggered());

        let done = cb.push(make_array(4));
        assert!(done);

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
        assert!(done);

        let captured = cb.take_captured();
        // 1 pre (id=1) + 2 post (id=2 triggering frame + id=3)
        assert_eq!(captured.len(), 3);
        assert_eq!(captured[0].unique_id, 1);
        assert_eq!(captured[1].unique_id, 2);
        assert_eq!(captured[2].unique_id, 3);
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
        assert!(done);
        assert_eq!(cb.status(), BufferStatus::BufferFilling); // back to filling after first capture

        cb.take_captured();

        // Refill buffer
        cb.push(make_array(3));

        // Second trigger — should reach preset count
        cb.trigger();
        assert_eq!(cb.trigger_count(), 2);
        assert_eq!(cb.status(), BufferStatus::Flushing);

        let done = cb.push(make_array(4));
        assert!(done);
        assert_eq!(cb.status(), BufferStatus::AcquisitionCompleted);

        cb.take_captured();

        // Further frames should be ignored
        let done = cb.push(make_array(5));
        assert!(!done);
        assert_eq!(cb.status(), BufferStatus::AcquisitionCompleted);

        // Further triggers should be ignored
        cb.trigger();
        assert_eq!(cb.trigger_count(), 2); // unchanged
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
        assert!(done);
        assert_eq!(cb.status(), BufferStatus::BufferFilling);

        // Reset -> Idle
        cb.reset();
        assert_eq!(cb.status(), BufferStatus::Idle);
        assert_eq!(cb.trigger_count(), 0);
    }
}
