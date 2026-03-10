/// Lowered IR — the intermediate representation between AST and Rust codegen.
///
/// IR is a simplified, flat representation of the SNL program:
/// - All names are resolved to indices
/// - All types are concrete
/// - No expression parsing needed (expressions stored as string fragments for now)

/// Top-level IR for an entire SNL program.
#[derive(Debug, Clone)]
pub struct SeqIR {
    pub program_name: String,
    pub options: ProgramOptions,
    pub channels: Vec<IRChannel>,
    pub event_flags: Vec<IREventFlag>,
    pub variables: Vec<IRVariable>,
    pub state_sets: Vec<IRStateSet>,
    pub entry_block: Option<IRBlock>,
    pub exit_block: Option<IRBlock>,
}

#[derive(Debug, Clone, Default)]
pub struct ProgramOptions {
    pub safe_mode: bool,
    pub reentrant: bool,
    pub main_flag: bool,
}

#[derive(Debug, Clone)]
pub struct IRChannel {
    pub id: usize,
    pub var_name: String,
    pub pv_name: String,
    pub var_type: IRType,
    pub monitored: bool,
    pub sync_ef: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct IREventFlag {
    pub id: usize,
    pub name: String,
    pub synced_channels: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct IRVariable {
    pub name: String,
    pub var_type: IRType,
    /// If assigned to a channel, the channel id.
    pub channel_id: Option<usize>,
    pub init_value: Option<String>,
}

/// Supported variable types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IRType {
    Int,
    Short,
    Long,
    Float,
    Double,
    String,
    Char,
    Array {
        element: Box<IRType>,
        size: usize,
    },
}

impl IRType {
    /// Rust type name for codegen.
    pub fn rust_type(&self) -> String {
        match self {
            IRType::Int => "i32".into(),
            IRType::Short => "i16".into(),
            IRType::Long => "i32".into(),
            IRType::Float => "f32".into(),
            IRType::Double => "f64".into(),
            IRType::String => "String".into(),
            IRType::Char => "u8".into(),
            IRType::Array { element, size } => {
                format!("[{}; {size}]", element.rust_type())
            }
        }
    }

    /// Default value expression for codegen.
    pub fn default_value(&self) -> String {
        match self {
            IRType::Int | IRType::Short | IRType::Long => "0".into(),
            IRType::Float => "0.0f32".into(),
            IRType::Double => "0.0".into(),
            IRType::String => "String::new()".into(),
            IRType::Char => "0u8".into(),
            IRType::Array { element, size } => {
                format!("[{}; {size}]", element.default_value())
            }
        }
    }

    /// EpicsValue constructor for codegen.
    pub fn to_epics_value_expr(&self, var_expr: &str) -> String {
        match self {
            IRType::Double => format!("EpicsValue::Double({var_expr})"),
            IRType::Float => format!("EpicsValue::Float({var_expr})"),
            IRType::Int | IRType::Long => format!("EpicsValue::Long({var_expr})"),
            IRType::Short => format!("EpicsValue::Short({var_expr})"),
            IRType::String => format!("EpicsValue::String({var_expr}.clone())"),
            IRType::Char => format!("EpicsValue::Char({var_expr})"),
            IRType::Array { element, .. } => {
                match element.as_ref() {
                    IRType::Double => format!("EpicsValue::DoubleArray({var_expr}.to_vec())"),
                    IRType::Float => format!("EpicsValue::FloatArray({var_expr}.to_vec())"),
                    IRType::Int | IRType::Long => format!("EpicsValue::LongArray({var_expr}.to_vec())"),
                    IRType::Short => format!("EpicsValue::ShortArray({var_expr}.to_vec())"),
                    _ => format!("EpicsValue::Double(0.0) /* unsupported array type */"),
                }
            }
        }
    }

    /// Expression to extract from EpicsValue for codegen.
    pub fn from_epics_value_expr(&self, val_expr: &str) -> String {
        match self {
            IRType::Double => format!("{val_expr}.to_f64().unwrap_or(0.0)"),
            IRType::Float => format!("{val_expr}.to_f64().unwrap_or(0.0) as f32"),
            IRType::Int | IRType::Long => format!("{val_expr}.to_f64().unwrap_or(0.0) as i32"),
            IRType::Short => format!("{val_expr}.to_f64().unwrap_or(0.0) as i16"),
            IRType::Char => format!("{val_expr}.to_f64().unwrap_or(0.0) as u8"),
            IRType::String => format!("format!(\"{{}}\", {val_expr})"),
            IRType::Array { element, size } => {
                let elem_type = element.rust_type();
                let elem_extract = match element.as_ref() {
                    IRType::Double => format!(
                        "{{ let arr = {val_expr}.to_f64_array(); let mut out = [0.0{elem_type}; {size}]; let n = arr.len().min({size}); out[..n].copy_from_slice(&arr[..n]); out }}"
                    ),
                    _ => format!(
                        "[{default}; {size}] /* array extract not fully supported */",
                        default = element.default_value()
                    ),
                };
                elem_extract
            }
        }
    }

    /// Get the element type for arrays.
    pub fn element_type(&self) -> Option<&IRType> {
        match self {
            IRType::Array { element, .. } => Some(element),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IRStateSet {
    pub name: String,
    pub id: usize,
    pub local_vars: Vec<IRVariable>,
    pub states: Vec<IRState>,
}

#[derive(Debug, Clone)]
pub struct IRState {
    pub name: String,
    pub id: usize,
    pub entry: Option<IRBlock>,
    pub transitions: Vec<IRTransition>,
    pub exit: Option<IRBlock>,
}

#[derive(Debug, Clone)]
pub struct IRTransition {
    /// Condition expression as Rust code string.
    /// None means unconditional (always true).
    pub condition: Option<String>,
    /// Action block as Rust code string.
    pub action: IRBlock,
    /// Target state index, or None for `exit`.
    pub target_state: Option<usize>,
}

/// A code block — stored as a string of Rust code for now.
/// The codegen will emit this verbatim into the generated function.
#[derive(Debug, Clone)]
pub struct IRBlock {
    pub code: String,
}

impl IRBlock {
    pub fn new(code: impl Into<String>) -> Self {
        Self { code: code.into() }
    }
}
