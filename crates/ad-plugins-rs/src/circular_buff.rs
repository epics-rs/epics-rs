use std::collections::VecDeque;
use std::sync::Arc;

use ad_core_rs::ndarray::NDArray;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};

/// Operations supported by CalcExpression.
#[derive(Debug, Clone, Copy)]
enum CalcOp {
    Gt,
    Lt,
    Ge,
    Le,
    Eq,
    Ne,
    And,
    Or,
    Not,
}

/// Token in a parsed CalcExpression (RPN).
#[derive(Debug, Clone)]
enum CalcToken {
    Num(f64),
    VarA,
    VarB,
    Op(CalcOp),
}

/// Raw token used during parsing (before conversion to RPN).
#[derive(Debug, Clone)]
enum RawToken {
    Num(f64),
    VarA,
    VarB,
    Op(CalcOp),
    LParen,
    RParen,
}

/// A simple expression evaluator supporting variables A and B,
/// numeric literals, comparison and logical operators.
///
/// Parsed into reverse-polish notation (RPN) using shunting-yard.
#[derive(Debug, Clone)]
pub struct CalcExpression {
    tokens: Vec<CalcToken>,
}

impl CalcExpression {
    /// Parse an infix expression into RPN.
    ///
    /// Supports: A, B (variables), numeric literals (including decimals and negatives
    /// at start or after open paren), >, <, >=, <=, ==, !=, &&, ||, !, parentheses.
    pub fn parse(expr: &str) -> Option<CalcExpression> {
        let raw_tokens = Self::tokenize(expr)?;
        let rpn = Self::shunting_yard(raw_tokens)?;
        Some(CalcExpression { tokens: rpn })
    }

    /// Evaluate the expression with the given variable values.
    /// Returns the numeric result; nonzero means true for trigger purposes.
    pub fn evaluate(&self, a: f64, b: f64) -> f64 {
        let mut stack: Vec<f64> = Vec::new();
        for tok in &self.tokens {
            match tok {
                CalcToken::Num(n) => stack.push(*n),
                CalcToken::VarA => stack.push(a),
                CalcToken::VarB => stack.push(b),
                CalcToken::Op(op) => match op {
                    CalcOp::Not => {
                        let v = stack.pop().unwrap_or(0.0);
                        stack.push(if v == 0.0 { 1.0 } else { 0.0 });
                    }
                    _ => {
                        let rhs = stack.pop().unwrap_or(0.0);
                        let lhs = stack.pop().unwrap_or(0.0);
                        let result = match op {
                            CalcOp::Gt => {
                                if lhs > rhs {
                                    1.0
                                } else {
                                    0.0
                                }
                            }
                            CalcOp::Lt => {
                                if lhs < rhs {
                                    1.0
                                } else {
                                    0.0
                                }
                            }
                            CalcOp::Ge => {
                                if lhs >= rhs {
                                    1.0
                                } else {
                                    0.0
                                }
                            }
                            CalcOp::Le => {
                                if lhs <= rhs {
                                    1.0
                                } else {
                                    0.0
                                }
                            }
                            CalcOp::Eq => {
                                if (lhs - rhs).abs() < f64::EPSILON {
                                    1.0
                                } else {
                                    0.0
                                }
                            }
                            CalcOp::Ne => {
                                if (lhs - rhs).abs() >= f64::EPSILON {
                                    1.0
                                } else {
                                    0.0
                                }
                            }
                            CalcOp::And => {
                                if lhs != 0.0 && rhs != 0.0 {
                                    1.0
                                } else {
                                    0.0
                                }
                            }
                            CalcOp::Or => {
                                if lhs != 0.0 || rhs != 0.0 {
                                    1.0
                                } else {
                                    0.0
                                }
                            }
                            CalcOp::Not => unreachable!(),
                        };
                        stack.push(result);
                    }
                },
            }
        }
        stack.pop().unwrap_or(0.0)
    }

    fn precedence(op: &CalcOp) -> u8 {
        match op {
            CalcOp::Or => 1,
            CalcOp::And => 2,
            CalcOp::Eq | CalcOp::Ne => 3,
            CalcOp::Gt | CalcOp::Lt | CalcOp::Ge | CalcOp::Le => 4,
            CalcOp::Not => 5,
        }
    }

    fn is_right_assoc(op: &CalcOp) -> bool {
        matches!(op, CalcOp::Not)
    }

    fn tokenize(expr: &str) -> Option<Vec<RawToken>> {
        use RawToken as RT;
        let chars: Vec<char> = expr.chars().collect();
        let mut tokens = Vec::new();
        let mut i = 0;

        while i < chars.len() {
            match chars[i] {
                ' ' | '\t' => {
                    i += 1;
                }
                '(' => {
                    tokens.push(RT::LParen);
                    i += 1;
                }
                ')' => {
                    tokens.push(RT::RParen);
                    i += 1;
                }
                'A' | 'a' => {
                    tokens.push(RT::VarA);
                    i += 1;
                }
                'B' | 'b' => {
                    tokens.push(RT::VarB);
                    i += 1;
                }
                '>' => {
                    if i + 1 < chars.len() && chars[i + 1] == '=' {
                        tokens.push(RT::Op(CalcOp::Ge));
                        i += 2;
                    } else {
                        tokens.push(RT::Op(CalcOp::Gt));
                        i += 1;
                    }
                }
                '<' => {
                    if i + 1 < chars.len() && chars[i + 1] == '=' {
                        tokens.push(RT::Op(CalcOp::Le));
                        i += 2;
                    } else {
                        tokens.push(RT::Op(CalcOp::Lt));
                        i += 1;
                    }
                }
                '=' => {
                    if i + 1 < chars.len() && chars[i + 1] == '=' {
                        tokens.push(RT::Op(CalcOp::Eq));
                        i += 2;
                    } else {
                        return None; // Single '=' not supported
                    }
                }
                '!' => {
                    if i + 1 < chars.len() && chars[i + 1] == '=' {
                        tokens.push(RT::Op(CalcOp::Ne));
                        i += 2;
                    } else {
                        tokens.push(RT::Op(CalcOp::Not));
                        i += 1;
                    }
                }
                '&' => {
                    if i + 1 < chars.len() && chars[i + 1] == '&' {
                        tokens.push(RT::Op(CalcOp::And));
                        i += 2;
                    } else {
                        return None;
                    }
                }
                '|' => {
                    if i + 1 < chars.len() && chars[i + 1] == '|' {
                        tokens.push(RT::Op(CalcOp::Or));
                        i += 2;
                    } else {
                        return None;
                    }
                }
                c if c.is_ascii_digit() || c == '.' => {
                    let start = i;
                    while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                        i += 1;
                    }
                    let num_str: String = chars[start..i].iter().collect();
                    let num: f64 = num_str.parse().ok()?;
                    tokens.push(RT::Num(num));
                }
                '-' => {
                    // Negative number: at start, or after '(' or after an operator
                    let is_unary_minus = tokens.is_empty()
                        || matches!(tokens.last(), Some(RT::LParen) | Some(RT::Op(_)));
                    if is_unary_minus
                        && i + 1 < chars.len()
                        && (chars[i + 1].is_ascii_digit() || chars[i + 1] == '.')
                    {
                        i += 1; // skip '-'
                        let start = i;
                        while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                            i += 1;
                        }
                        let num_str: String = chars[start..i].iter().collect();
                        let num: f64 = num_str.parse().ok()?;
                        tokens.push(RT::Num(-num));
                    } else {
                        return None; // Subtraction not supported
                    }
                }
                _ => return None,
            }
        }

        Some(tokens)
    }

    fn shunting_yard(raw: Vec<RawToken>) -> Option<Vec<CalcToken>> {
        use RawToken as RT;
        let mut output: Vec<CalcToken> = Vec::new();
        let mut op_stack: Vec<RawToken> = Vec::new();

        for tok in raw {
            match tok {
                RT::Num(n) => output.push(CalcToken::Num(n)),
                RT::VarA => output.push(CalcToken::VarA),
                RT::VarB => output.push(CalcToken::VarB),
                RT::Op(ref op) => {
                    while let Some(RT::Op(top_op)) = op_stack.last() {
                        let top_prec = Self::precedence(top_op);
                        let cur_prec = Self::precedence(op);
                        if (!Self::is_right_assoc(op) && cur_prec <= top_prec)
                            || (Self::is_right_assoc(op) && cur_prec < top_prec)
                        {
                            if let Some(RT::Op(o)) = op_stack.pop() {
                                output.push(CalcToken::Op(o));
                            }
                        } else {
                            break;
                        }
                    }
                    op_stack.push(tok);
                }
                RT::LParen => op_stack.push(tok),
                RT::RParen => {
                    loop {
                        match op_stack.pop() {
                            Some(RT::LParen) => break,
                            Some(RT::Op(o)) => output.push(CalcToken::Op(o)),
                            _ => return None, // Mismatched parens
                        }
                    }
                }
            }
        }

        // Pop remaining operators
        while let Some(tok) = op_stack.pop() {
            match tok {
                RT::Op(o) => output.push(CalcToken::Op(o)),
                RT::LParen => return None, // Mismatched parens
                _ => return None,
            }
        }

        Some(output)
    }
}

/// Trigger condition for circular buffer.
#[derive(Debug, Clone)]
pub enum TriggerCondition {
    /// Trigger on an attribute value exceeding threshold.
    AttributeThreshold { name: String, threshold: f64 },
    /// External trigger (manual).
    External,
    /// Calculated trigger based on two attribute values and an expression.
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
    pre_count: usize,
    post_count: usize,
    buffer: VecDeque<Arc<NDArray>>,
    trigger_condition: TriggerCondition,
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
    status: BufferStatus,
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

        // Check trigger condition
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
                    .unwrap_or(0.0);
                let b = array
                    .attributes
                    .get(attr_b)
                    .and_then(|a| a.value.as_f64())
                    .unwrap_or(0.0);
                expression.evaluate(a, b) != 0.0
            }
        };

        // Maintain pre-trigger ring buffer
        self.buffer.push_back(array);
        if self.buffer.len() > self.pre_count {
            self.buffer.pop_front();
        }

        if trigger {
            self.trigger();
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
pub struct CircularBuffProcessor {
    buffer: CircularBuffer,
}

impl CircularBuffProcessor {
    pub fn new(pre_count: usize, post_count: usize, condition: TriggerCondition) -> Self {
        Self {
            buffer: CircularBuffer::new(pre_count, post_count, condition),
        }
    }

    pub fn trigger(&mut self) {
        self.buffer.trigger();
    }

    pub fn buffer(&self) -> &CircularBuffer {
        &self.buffer
    }
}

impl NDPluginProcess for CircularBuffProcessor {
    fn process_array(&mut self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        let done = self.buffer.push(Arc::new(array.clone()));
        if done {
            ProcessResult::arrays(self.buffer.take_captured())
        } else {
            ProcessResult::empty()
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
        Ok(())
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
            1,
            TriggerCondition::AttributeThreshold {
                name: "trigger".into(),
                threshold: 5.0,
            },
        );

        cb.push(make_array_with_attr(1, 1.0));
        cb.push(make_array_with_attr(2, 2.0));
        assert!(!cb.is_triggered());

        // This should trigger (attr >= 5.0)
        cb.push(make_array_with_attr(3, 5.0));
        assert!(cb.is_triggered());

        let done = cb.push(make_array(4));
        assert!(done);

        let captured = cb.take_captured();
        assert_eq!(captured.len(), 2); // 1 pre + 1 post
    }

    // --- New tests ---

    #[test]
    fn test_calc_trigger() {
        // Expression: "A>5" — trigger when attribute A exceeds 5
        let expr = CalcExpression::parse("A>5").unwrap();
        let mut cb = CircularBuffer::new(
            1,
            1,
            TriggerCondition::Calc {
                attr_a: "attr_a".into(),
                attr_b: "attr_b".into(),
                expression: expr,
            },
        );

        // A=3, should not trigger
        cb.push(make_array_with_attrs(1, 3.0, 0.0));
        assert!(!cb.is_triggered());

        // A=6, should trigger
        cb.push(make_array_with_attrs(2, 6.0, 0.0));
        assert!(cb.is_triggered());

        let done = cb.push(make_array(3));
        assert!(done);

        let captured = cb.take_captured();
        assert_eq!(captured.len(), 2); // 1 pre + 1 post
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

        // Invalid expression returns None
        assert!(CalcExpression::parse("A=5").is_none()); // single = not supported
        assert!(CalcExpression::parse("A&B").is_none()); // single & not supported
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
