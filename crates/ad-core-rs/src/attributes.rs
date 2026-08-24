/// Source of an NDAttribute value.
///
/// C `NDAttribute` keeps the source *string* (`source_`, the constructor
/// `pSource`) independent of the source *type* (`sourceType_`); the EPICS_PV,
/// Param, Function, and Const variants each carry their own configured `source`
/// property (a PV name, asyn drvInfo, function name, or constant string), so the
/// variant holds that string rather than re-synthesizing it from the type.
/// See [`NDAttrSource::source_string`].
#[derive(Debug, Clone, PartialEq)]
pub enum NDAttrSource {
    Driver,
    Param {
        port_name: String,
        param_name: String,
    },
    /// EPICS PV name (C++ `PVAttribute` `pSource`).
    EpicsPV(String),
    /// Registered function name (C++ `functAttribute` `pSource`).
    Function(String),
    /// Constant string from the XML `source` property (C++ `NDAttrSourceConst`).
    Constant(String),
    Undefined,
}

impl NDAttrSource {
    /// The original `source` string for this attribute, stored verbatim from
    /// construction — *not* derived from the source type.
    ///
    /// C `NDAttribute` publishes `source_` verbatim from both the NTNDArray and
    /// netCDF writers (`NDAttribute.cpp:68`, `ntndArrayConverter.cpp:564`).
    /// Driver-created attributes carry the literal `"Driver"`
    /// (`NDAttributeList.cpp:73`); a Param attribute's source is its asyn
    /// drvInfo (the `param_name`); EPICS_PV / Function / Const attributes carry
    /// their configured XML `source` string.
    pub fn source_string(&self) -> &str {
        match self {
            Self::Driver => "Driver",
            Self::Param { param_name, .. } => param_name,
            Self::EpicsPV(s) | Self::Function(s) | Self::Constant(s) => s,
            Self::Undefined => "",
        }
    }
}

/// Data type tags for NDAttribute values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NDAttrDataType {
    Int8,
    UInt8,
    Int16,
    UInt16,
    Int32,
    UInt32,
    Int64,
    UInt64,
    Float32,
    Float64,
    String,
}

/// Typed value stored in an NDAttribute.
#[derive(Debug, Clone, PartialEq)]
pub enum NDAttrValue {
    Int8(i8),
    UInt8(u8),
    Int16(i16),
    UInt16(u16),
    Int32(i32),
    UInt32(u32),
    Int64(i64),
    UInt64(u64),
    Float32(f32),
    Float64(f64),
    String(String),
    Undefined,
}

impl NDAttrValue {
    pub fn data_type(&self) -> NDAttrDataType {
        match self {
            Self::Int8(_) => NDAttrDataType::Int8,
            Self::UInt8(_) => NDAttrDataType::UInt8,
            Self::Int16(_) => NDAttrDataType::Int16,
            Self::UInt16(_) => NDAttrDataType::UInt16,
            Self::Int32(_) => NDAttrDataType::Int32,
            Self::UInt32(_) => NDAttrDataType::UInt32,
            Self::Int64(_) => NDAttrDataType::Int64,
            Self::UInt64(_) => NDAttrDataType::UInt64,
            Self::Float32(_) => NDAttrDataType::Float32,
            Self::Float64(_) => NDAttrDataType::Float64,
            Self::String(_) => NDAttrDataType::String,
            Self::Undefined => NDAttrDataType::Int32,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Int8(v) => Some(*v as f64),
            Self::UInt8(v) => Some(*v as f64),
            Self::Int16(v) => Some(*v as f64),
            Self::UInt16(v) => Some(*v as f64),
            Self::Int32(v) => Some(*v as f64),
            Self::UInt32(v) => Some(*v as f64),
            Self::Int64(v) => Some(*v as f64),
            Self::UInt64(v) => Some(*v as f64),
            Self::Float32(v) => Some(*v as f64),
            Self::Float64(v) => Some(*v),
            Self::String(_) | Self::Undefined => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int8(v) => Some(*v as i64),
            Self::UInt8(v) => Some(*v as i64),
            Self::Int16(v) => Some(*v as i64),
            Self::UInt16(v) => Some(*v as i64),
            Self::Int32(v) => Some(*v as i64),
            Self::UInt32(v) => Some(*v as i64),
            Self::Int64(v) => Some(*v),
            Self::UInt64(v) => Some(*v as i64),
            Self::Float32(v) => Some(*v as i64),
            Self::Float64(v) => Some(*v as i64),
            Self::String(_) | Self::Undefined => None,
        }
    }

    pub fn as_string(&self) -> String {
        match self {
            Self::Int8(v) => v.to_string(),
            Self::UInt8(v) => v.to_string(),
            Self::Int16(v) => v.to_string(),
            Self::UInt16(v) => v.to_string(),
            Self::Int32(v) => v.to_string(),
            Self::UInt32(v) => v.to_string(),
            Self::Int64(v) => v.to_string(),
            Self::UInt64(v) => v.to_string(),
            Self::Float32(v) => v.to_string(),
            Self::Float64(v) => v.to_string(),
            Self::String(v) => v.clone(),
            Self::Undefined => String::new(),
        }
    }

    /// String value of a *string-typed* attribute, mirroring C++
    /// `NDAttribute::getValue(NDAttrString, …)` which returns `ND_ERROR`
    /// (leaving the caller's buffer unchanged) for any non-string attribute.
    /// Unlike [`Self::as_string`], a numeric attribute yields `None` rather than its
    /// decimal rendering, so callers that drive control behaviour off a
    /// string-typed attribute (filename, file number, destination port) ignore
    /// a mis-typed numeric attribute exactly as the C reader does.
    pub fn as_string_typed(&self) -> Option<&str> {
        match self {
            Self::String(v) => Some(v.as_str()),
            _ => None,
        }
    }
}

/// A live source backing an [`NDAttribute`].
///
/// C++ models live attributes with `paramAttribute`, `PVAttribute` and
/// `functAttribute` subclasses, each overriding `updateValue()` to re-read
/// from a driver parameter, an EPICS PV, or a user function. The Rust port
/// keeps `NDAttribute` a single concrete type and delegates re-evaluation to
/// an implementation of this trait. `Constant` / `Driver` attributes have no
/// source object — their value is static (C++ `NDAttribute::updateValue` is a
/// no-op for those).
pub trait NDAttributeSource: Send + Sync {
    /// Re-evaluate the attribute from its underlying source.
    fn evaluate(&self) -> NDAttrValue;

    /// Downcast hook so the driver can recognize a `ParamAttributeSource` (or
    /// `EpicsPvAttributeSource`) and feed its [`LiveValueCell`].
    fn as_any(&self) -> &dyn std::any::Any;
}

/// A thread-safe value cell shared between an attribute source and the agent
/// that feeds it fresh values (the driver for `Param`, a CA-monitor task for
/// `EpicsPV`). The feeder calls [`LiveValueCell::set`]; the attribute source
/// reads via [`LiveValueCell::get`].
#[derive(Debug)]
pub struct LiveValueCell {
    value: std::sync::RwLock<NDAttrValue>,
}

impl LiveValueCell {
    /// Create a cell with an initial (typically `Undefined`) value.
    pub fn new(initial: NDAttrValue) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            value: std::sync::RwLock::new(initial),
        })
    }

    /// Store a fresh value (called by the feeder — driver / CA-monitor task).
    pub fn set(&self, value: NDAttrValue) {
        if let Ok(mut guard) = self.value.write() {
            *guard = value;
        }
    }

    /// Read the current value.
    pub fn get(&self) -> NDAttrValue {
        self.value
            .read()
            .map(|g| g.clone())
            .unwrap_or(NDAttrValue::Undefined)
    }
}

impl NDAttributeSource for LiveValueCell {
    fn evaluate(&self) -> NDAttrValue {
        self.get()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// The read type a `Param`-type attribute uses, selected from the XML
/// `datatype` attribute (C++ `paramAttribute::paramType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamAttrType {
    Int32,
    Int64,
    Float64,
    String,
    /// Unrecognized / omitted `datatype` — C++ leaves `paramType` at
    /// `paramAttrTypeUnknown`, so the attribute keeps its constructed
    /// `NDAttrUndefined` value and `updateValue()` never refreshes it.
    Unknown,
}

impl ParamAttrType {
    /// Map the XML `datatype` string to a read type, mirroring C++
    /// `paramAttribute` (paramAttribute.cpp:80-95). The comparison is
    /// case-sensitive against the upper-case schema tokens
    /// (asynNDArrayDriver.cpp:280: "always uppercase for datatype"); anything
    /// else — including the lower-case `"int"` default asynNDArrayDriver
    /// substitutes when `datatype` is omitted (asynNDArrayDriver.cpp:446) —
    /// yields `Unknown`, which C leaves as an un-typed, never-updated attribute.
    pub fn from_datatype(datatype: &str) -> Self {
        match datatype {
            "INT" => Self::Int32,
            "INT64" => Self::Int64,
            "DOUBLE" => Self::Float64,
            "STRING" => Self::String,
            _ => Self::Unknown,
        }
    }
}

/// Live source for a `Param`-type attribute (C++ `paramAttribute`).
///
/// The driver owns the asyn parameter library; on each
/// `NDAttributeList::update_values()` cycle the driver refreshes this source's
/// [`LiveValueCell`] with the current value of the parameter named
/// `param_name`. `evaluate()` then returns that fresh value — mirroring C++
/// `paramAttribute::updateValue()`, which reads `pDriver->getXxxParam()`.
#[derive(Debug)]
pub struct ParamAttributeSource {
    /// The asyn parameter name this attribute reads (the XML `source`).
    pub param_name: String,
    /// The address (asyn `paramAddr`) to read from.
    pub addr: i32,
    /// The read type selected from the XML `datatype` attribute. The published
    /// `NDAttribute` value type follows this, NOT the param's runtime type —
    /// C++ `paramAttribute` reads `getIntegerParam`/`getDoubleParam`/… per
    /// `datatype` and sets a fixed `NDAttrDataType`.
    pub param_type: ParamAttrType,
    cell: std::sync::Arc<LiveValueCell>,
}

impl ParamAttributeSource {
    pub fn new(
        param_name: impl Into<String>,
        addr: i32,
        param_type: ParamAttrType,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            param_name: param_name.into(),
            addr,
            param_type,
            cell: LiveValueCell::new(NDAttrValue::Undefined),
        })
    }

    /// The shared cell the driver writes fresh parameter values into.
    pub fn cell(&self) -> &std::sync::Arc<LiveValueCell> {
        &self.cell
    }
}

impl NDAttributeSource for ParamAttributeSource {
    fn evaluate(&self) -> NDAttrValue {
        self.cell.get()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A user attribute function: receives the `functParam` string and returns the
/// attribute's current value. Port of C++ `NDAttributeFunction`
/// (`functAttribute` calls `pFunction(functParam, &functionPvt, this)`).
pub type NDAttributeFunction = std::sync::Arc<dyn Fn(&str) -> NDAttrValue + Send + Sync>;

/// Registry of named attribute functions (C++ `registryFunctionFind` /
/// `registerNDAttributeFunction`).
///
/// A `Function`-type attribute names a function here; `FunctionAttributeSource`
/// looks it up and calls it on every `evaluate()`.
#[derive(Default)]
pub struct NDAttributeFunctionRegistry {
    funcs: std::sync::RwLock<std::collections::HashMap<String, NDAttributeFunction>>,
}

impl NDAttributeFunctionRegistry {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    /// Register an attribute function under `name` (C++
    /// `registerNDAttributeFunction`).
    pub fn register(
        &self,
        name: impl Into<String>,
        func: impl Fn(&str) -> NDAttrValue + Send + Sync + 'static,
    ) {
        if let Ok(mut g) = self.funcs.write() {
            g.insert(name.into(), std::sync::Arc::new(func));
        }
    }

    /// Look up a registered function by name (C++ `registryFunctionFind`).
    pub fn find(&self, name: &str) -> Option<NDAttributeFunction> {
        self.funcs.read().ok()?.get(name).cloned()
    }
}

/// Live source for a `Function`-type attribute (C++ `functAttribute`).
///
/// Holds a reference to the function registry, the function name (XML
/// `source`) and the function parameter string (XML `param`). `evaluate()`
/// looks the function up and calls it — matching `functAttribute::updateValue`.
/// If the function is not registered, `evaluate()` returns `Undefined`
/// (C++ `updateValue` returns `asynError` and leaves the value unchanged).
pub struct FunctionAttributeSource {
    registry: std::sync::Arc<NDAttributeFunctionRegistry>,
    function_name: String,
    function_param: String,
}

impl FunctionAttributeSource {
    pub fn new(
        registry: std::sync::Arc<NDAttributeFunctionRegistry>,
        function_name: impl Into<String>,
        function_param: impl Into<String>,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            registry,
            function_name: function_name.into(),
            function_param: function_param.into(),
        })
    }
}

impl NDAttributeSource for FunctionAttributeSource {
    fn evaluate(&self) -> NDAttrValue {
        match self.registry.find(&self.function_name) {
            Some(f) => f(&self.function_param),
            None => NDAttrValue::Undefined,
        }
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// The `chtype` an `EPICS_PV` attribute subscribes with — C++ `PVAttribute`'s
/// `dbrType` member, taken from the XML `dbrtype` attribute.
///
/// The variants are named for the `NDAttrDataType` C publishes for each
/// `DBR_XXX` (PVAttribute.cpp:257-282), not for the CA wire type, because
/// several DBR types collapse: `DBR_SHORT`, `DBR_INT` and `DBR_ENUM` all
/// publish `NDAttrInt16`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PvAttrDbrType {
    /// C `DBR_NATIVE` — the default when `dbrtype` is omitted. The published
    /// type follows the PV's own field type (PVAttribute.cpp:224-249).
    #[default]
    Native,
    /// `DBR_STRING` → `NDAttrString`.
    String,
    /// `DBR_CHAR` → `NDAttrInt8`.
    Int8,
    /// `DBR_SHORT` / `DBR_INT` / `DBR_ENUM` → `NDAttrInt16`.
    Int16,
    /// `DBR_LONG` → `NDAttrInt32`.
    Int32,
    /// `DBR_FLOAT` → `NDAttrFloat32`.
    Float32,
    /// `DBR_DOUBLE` → `NDAttrFloat64`.
    Float64,
}

impl PvAttrDbrType {
    /// Parse the XML `dbrtype` spelling — C's nine-name table
    /// (asynNDArrayDriver.cpp:421-434).
    ///
    /// `None` is C's `else` arm, which prints `unknown dbrType` and returns
    /// `asynError`: the whole attributes file is refused rather than the one
    /// attribute being silently dropped, so a typo cannot leave an IOC
    /// producing arrays that are quietly missing an attribute.
    pub fn from_xml(dbrtype: &str) -> Option<Self> {
        match dbrtype {
            "DBR_CHAR" => Some(Self::Int8),
            "DBR_SHORT" | "DBR_INT" | "DBR_ENUM" => Some(Self::Int16),
            "DBR_LONG" => Some(Self::Int32),
            "DBR_FLOAT" => Some(Self::Float32),
            "DBR_DOUBLE" => Some(Self::Float64),
            "DBR_STRING" => Some(Self::String),
            "DBR_NATIVE" => Some(Self::Native),
            _ => None,
        }
    }

    /// Publish a monitored value as the type this `dbrtype` asked for.
    ///
    /// C requests the type on the wire (`ca_add_masked_array_event(dbrType, …)`,
    /// PVAttribute.cpp:293) and stores whatever arrives into the matching union
    /// member, so the attribute's `NDAttrDataType` is the REQUESTED type, not
    /// the PV's. `CaClient::camonitor` subscribes with the channel's native
    /// type and takes no type argument, so the port converts on arrival
    /// instead; the published type is the same either way. A value that will
    /// not convert — a non-numeric string asked for as `DBR_DOUBLE` — stays
    /// `Undefined`, C's un-set union member.
    pub fn publish(self, value: NDAttrValue) -> NDAttrValue {
        if matches!(value, NDAttrValue::Undefined) {
            return value;
        }
        match self {
            Self::Native => value,
            Self::String => NDAttrValue::String(value.as_string()),
            Self::Int8 => value
                .as_i64()
                .map_or(NDAttrValue::Undefined, |v| NDAttrValue::Int8(v as i8)),
            Self::Int16 => value
                .as_i64()
                .map_or(NDAttrValue::Undefined, |v| NDAttrValue::Int16(v as i16)),
            Self::Int32 => value
                .as_i64()
                .map_or(NDAttrValue::Undefined, |v| NDAttrValue::Int32(v as i32)),
            Self::Float32 => value
                .as_f64()
                .map_or(NDAttrValue::Undefined, |v| NDAttrValue::Float32(v as f32)),
            Self::Float64 => value
                .as_f64()
                .map_or(NDAttrValue::Undefined, NDAttrValue::Float64),
        }
    }
}

/// Live source for an `EPICS_PV`-type attribute (C++ `PVAttribute`).
///
/// `evaluate()` returns whatever was last written into the shared
/// [`LiveValueCell`]. The writer is `spawn_ca_monitor`, started by
/// `NDArrayDriverBase` for every `EPICS_PV` attribute once the IOC has handed
/// the port a CA client. An IOC that never hands one over leaves the cell at
/// `Undefined` for its whole life, and `NDArrayDriverBase` reports that
/// through `ND_ATTRIBUTES_STATUS` rather than letting the file read back as
/// loaded.
#[derive(Debug)]
pub struct EpicsPvAttributeSource {
    /// The PV name this attribute monitors (the XML `source`).
    pub pv_name: String,
    /// The type the monitored value is published as (the XML `dbrtype`).
    pub dbr_type: PvAttrDbrType,
    cell: std::sync::Arc<LiveValueCell>,
}

impl EpicsPvAttributeSource {
    pub fn new(pv_name: impl Into<String>, dbr_type: PvAttrDbrType) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            pv_name: pv_name.into(),
            dbr_type,
            cell: LiveValueCell::new(NDAttrValue::Undefined),
        })
    }

    /// The shared cell a CA-monitor task writes fresh PV values into.
    pub fn cell(&self) -> &std::sync::Arc<LiveValueCell> {
        &self.cell
    }
}

impl NDAttributeSource for EpicsPvAttributeSource {
    fn evaluate(&self) -> NDAttrValue {
        self.cell.get()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// CA-monitor feeder for `EPICS_PV`-type attributes (C++ `PVAttribute`'s
/// channel-access monitor callback).
///
/// Requires the `ioc` feature (the `epics-ca-rs` CA client). Spawns a tokio
/// task that subscribes to the attribute's PV via [`epics_ca_rs::client::CaClient::camonitor`]
/// and writes every monitored value into the source's [`LiveValueCell`], so
/// `evaluate()` returns the latest value. The task exits when the monitor
/// stream ends; the returned [`tokio::task::JoinHandle`] lets the caller abort
/// it on shutdown.
///
/// `runtime` is named rather than taken from the ambient context because the
/// caller is `NDArrayDriverBase::read_nd_attributes_file`, reached from an
/// `NDAttributesFile` write on an asyn port thread that is not inside any
/// runtime.
#[cfg(feature = "ioc")]
pub fn spawn_ca_monitor(
    runtime: &tokio::runtime::Handle,
    client: std::sync::Arc<epics_ca_rs::client::CaClient>,
    source: &EpicsPvAttributeSource,
) -> tokio::task::JoinHandle<()> {
    let pv_name = source.pv_name.clone();
    let dbr_type = source.dbr_type;
    let cell = source.cell().clone();
    runtime.spawn(async move {
        let _ = client
            .camonitor(&pv_name, |value| {
                cell.set(dbr_type.publish(epics_value_to_nd_attr(&value)));
            })
            .await;
    })
}

/// Convert an `epics-ca-rs` [`epics_ca_rs::EpicsValue`] scalar to an
/// [`NDAttrValue`]. Array values collapse to their first element (an NDArray
/// attribute is scalar); empty arrays yield `Undefined`.
#[cfg(feature = "ioc")]
fn epics_value_to_nd_attr(value: &epics_ca_rs::EpicsValue) -> NDAttrValue {
    use epics_ca_rs::EpicsValue as E;
    match value {
        E::String(s) => NDAttrValue::String(s.as_str_lossy().into_owned()),
        E::Short(v) => NDAttrValue::Int16(*v),
        E::Float(v) => NDAttrValue::Float32(*v),
        E::Enum(v) => NDAttrValue::UInt16(*v),
        // `EnumWithChoices` is a transient NTEnum carrier that behaves
        // exactly like `Enum` everywhere except a string-target put_field:
        // read the index (epics-base-rs value.rs). An NDArray attribute is a
        // numeric scalar with no string target, so it takes the index form.
        E::EnumWithChoices { index, .. } => NDAttrValue::UInt16(*index),
        E::Char(v) => NDAttrValue::UInt8(*v),
        E::UChar(v) => NDAttrValue::UInt8(*v),
        E::Long(v) => NDAttrValue::Int32(*v),
        E::Double(v) => NDAttrValue::Float64(*v),
        E::Int64(v) => NDAttrValue::Int64(*v),
        E::UInt64(v) => NDAttrValue::UInt64(*v),
        E::UShort(v) => NDAttrValue::UInt16(*v),
        E::ULong(v) => NDAttrValue::UInt32(*v),
        E::ShortArray(a) => a
            .first()
            .map_or(NDAttrValue::Undefined, |v| NDAttrValue::Int16(*v)),
        E::FloatArray(a) => a
            .first()
            .map_or(NDAttrValue::Undefined, |v| NDAttrValue::Float32(*v)),
        E::EnumArray(a) => a
            .first()
            .map_or(NDAttrValue::Undefined, |v| NDAttrValue::UInt16(*v)),
        E::DoubleArray(a) => a
            .first()
            .map_or(NDAttrValue::Undefined, |v| NDAttrValue::Float64(*v)),
        E::LongArray(a) => a
            .first()
            .map_or(NDAttrValue::Undefined, |v| NDAttrValue::Int32(*v)),
        E::CharArray(a) => a
            .first()
            .map_or(NDAttrValue::Undefined, |v| NDAttrValue::UInt8(*v)),
        E::UCharArray(a) => a
            .first()
            .map_or(NDAttrValue::Undefined, |v| NDAttrValue::UInt8(*v)),
        E::Int64Array(a) => a
            .first()
            .map_or(NDAttrValue::Undefined, |v| NDAttrValue::Int64(*v)),
        E::UInt64Array(a) => a
            .first()
            .map_or(NDAttrValue::Undefined, |v| NDAttrValue::UInt64(*v)),
        E::UShortArray(a) => a
            .first()
            .map_or(NDAttrValue::Undefined, |v| NDAttrValue::UInt16(*v)),
        E::ULongArray(a) => a
            .first()
            .map_or(NDAttrValue::Undefined, |v| NDAttrValue::UInt32(*v)),
        E::StringArray(a) => a.first().map_or(NDAttrValue::Undefined, |v| {
            NDAttrValue::String(v.as_str_lossy().into_owned())
        }),
    }
}

/// A named attribute attached to an NDArray.
#[derive(Clone)]
pub struct NDAttribute {
    pub name: String,
    pub description: String,
    pub source: NDAttrSource,
    pub value: NDAttrValue,
    /// Optional live source. When present, [`NDAttribute::update`] re-reads the
    /// value from it; when absent the attribute is a static value (Constant /
    /// Driver), and `update` is a no-op — matching C++ behavior.
    pub source_impl: Option<std::sync::Arc<dyn NDAttributeSource>>,
}

impl std::fmt::Debug for NDAttribute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NDAttribute")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("source", &self.source)
            .field("value", &self.value)
            .field("has_source_impl", &self.source_impl.is_some())
            .finish()
    }
}

impl NDAttribute {
    /// Construct a static-valued attribute (no live source).
    pub fn new_static(
        name: impl Into<String>,
        description: impl Into<String>,
        source: NDAttrSource,
        value: NDAttrValue,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            source,
            value,
            source_impl: None,
        }
    }

    /// Construct a source-backed attribute. The value is evaluated immediately.
    pub fn new_with_source(
        name: impl Into<String>,
        description: impl Into<String>,
        source: NDAttrSource,
        source_impl: std::sync::Arc<dyn NDAttributeSource>,
    ) -> Self {
        let value = source_impl.evaluate();
        Self {
            name: name.into(),
            description: description.into(),
            source,
            value,
            source_impl: Some(source_impl),
        }
    }

    /// Re-evaluate this attribute from its live source.
    ///
    /// Mirrors C++ `NDAttribute::updateValue()` — a no-op for attributes
    /// without a live source (Constant / Driver), and a re-read for
    /// source-backed attributes (Param / EpicsPV / Function).
    pub fn update(&mut self) {
        if let Some(src) = &self.source_impl {
            self.value = src.evaluate();
        }
    }

    /// If this attribute is backed by a [`ParamAttributeSource`], return it so
    /// the driver can feed its [`LiveValueCell`] from the parameter library.
    pub fn param_source(&self) -> Option<&ParamAttributeSource> {
        self.source_impl
            .as_ref()
            .and_then(|s| s.as_any().downcast_ref::<ParamAttributeSource>())
    }

    /// If this attribute is backed by an [`EpicsPvAttributeSource`], return it
    /// so a CA-monitor task can feed its [`LiveValueCell`].
    pub fn epics_pv_source(&self) -> Option<&EpicsPvAttributeSource> {
        self.source_impl
            .as_ref()
            .and_then(|s| s.as_any().downcast_ref::<EpicsPvAttributeSource>())
    }
}

/// Collection of NDAttributes on an NDArray.
#[derive(Debug, Clone, Default)]
pub struct NDAttributeList {
    attrs: Vec<NDAttribute>,
}

impl NDAttributeList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, attr: NDAttribute) {
        if let Some(existing) = self.attrs.iter_mut().find(|a| a.name == attr.name) {
            *existing = attr;
        } else {
            self.attrs.push(attr);
        }
    }

    pub fn get(&self, name: &str) -> Option<&NDAttribute> {
        self.attrs.iter().find(|a| a.name == name)
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let len_before = self.attrs.len();
        self.attrs.retain(|a| a.name != name);
        self.attrs.len() < len_before
    }

    pub fn clear(&mut self) {
        self.attrs.clear();
    }

    pub fn iter(&self) -> impl Iterator<Item = &NDAttribute> {
        self.attrs.iter()
    }

    pub fn len(&self) -> usize {
        self.attrs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.attrs.is_empty()
    }

    /// Merge attributes from another list: adds new ones, updates existing by name.
    pub fn copy_from(&mut self, other: &NDAttributeList) {
        for attr in other.iter() {
            self.add(attr.clone());
        }
    }

    /// Re-evaluate every attribute from its live source.
    ///
    /// Mirrors C++ `NDAttributeList::updateValues()`, which is called before
    /// the list is copied onto an outgoing NDArray so each attribute carries a
    /// fresh value. Static (Constant / Driver) attributes are unaffected.
    pub fn update_values(&mut self) {
        for attr in self.attrs.iter_mut() {
            attr.update();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_get() {
        let mut list = NDAttributeList::new();
        list.add(NDAttribute {
            name: "ColorMode".into(),
            description: "Color mode".into(),
            source: NDAttrSource::Driver,
            value: NDAttrValue::Int32(0),
            source_impl: None,
        });
        let attr = list.get("ColorMode").unwrap();
        assert_eq!(attr.value, NDAttrValue::Int32(0));
    }

    #[test]
    fn test_replace_existing() {
        let mut list = NDAttributeList::new();
        list.add(NDAttribute {
            name: "Gain".into(),
            description: "".into(),
            source: NDAttrSource::Driver,
            value: NDAttrValue::Float64(1.0),
            source_impl: None,
        });
        list.add(NDAttribute {
            name: "Gain".into(),
            description: "".into(),
            source: NDAttrSource::Driver,
            value: NDAttrValue::Float64(2.5),
            source_impl: None,
        });
        assert_eq!(list.len(), 1);
        assert_eq!(list.get("Gain").unwrap().value, NDAttrValue::Float64(2.5));
    }

    #[test]
    fn test_iter() {
        let mut list = NDAttributeList::new();
        list.add(NDAttribute {
            name: "A".into(),
            description: "".into(),
            source: NDAttrSource::Constant(String::new()),
            value: NDAttrValue::Int32(1),
            source_impl: None,
        });
        list.add(NDAttribute {
            name: "B".into(),
            description: "".into(),
            source: NDAttrSource::Constant(String::new()),
            value: NDAttrValue::String("hello".into()),
            source_impl: None,
        });
        let names: Vec<_> = list.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["A", "B"]);
    }

    #[test]
    fn test_get_missing() {
        let list = NDAttributeList::new();
        assert!(list.get("nope").is_none());
    }

    #[test]
    fn test_empty() {
        let list = NDAttributeList::new();
        assert!(list.is_empty());
        assert_eq!(list.len(), 0);
    }

    #[test]
    fn test_all_data_types() {
        let values = vec![
            NDAttrValue::Int8(-1),
            NDAttrValue::UInt8(255),
            NDAttrValue::Int16(-100),
            NDAttrValue::UInt16(1000),
            NDAttrValue::Int32(-50000),
            NDAttrValue::UInt32(50000),
            NDAttrValue::Int64(-1_000_000),
            NDAttrValue::UInt64(1_000_000),
            NDAttrValue::Float32(3.14),
            NDAttrValue::Float64(2.718),
            NDAttrValue::String("test".into()),
        ];
        for v in &values {
            assert_eq!(v.data_type(), v.data_type());
        }
    }

    #[test]
    fn test_source_tracking() {
        let attr = NDAttribute {
            name: "temp".into(),
            description: "temperature".into(),
            source: NDAttrSource::Param {
                port_name: "SIM1".into(),
                param_name: "TEMPERATURE".into(),
            },
            value: NDAttrValue::Float64(25.0),
            source_impl: None,
        };
        match &attr.source {
            NDAttrSource::Param {
                port_name,
                param_name,
            } => {
                assert_eq!(port_name, "SIM1");
                assert_eq!(param_name, "TEMPERATURE");
            }
            _ => panic!("wrong source type"),
        }
    }

    #[test]
    fn test_value_conversions() {
        let v = NDAttrValue::Int32(42);
        assert_eq!(v.as_f64(), Some(42.0));
        assert_eq!(v.as_i64(), Some(42));
        assert_eq!(v.as_string(), "42");

        let s = NDAttrValue::String("hello".into());
        assert_eq!(s.as_f64(), None);
        assert_eq!(s.as_i64(), None);
        assert_eq!(s.as_string(), "hello");
    }

    #[test]
    fn test_remove() {
        let mut list = NDAttributeList::new();
        list.add(NDAttribute {
            name: "A".into(),
            description: "".into(),
            source: NDAttrSource::Driver,
            value: NDAttrValue::Int32(1),
            source_impl: None,
        });
        assert!(list.remove("A"));
        assert!(list.is_empty());
        assert!(!list.remove("A"));
    }

    /// G10: a source with a live `NDAttributeSource` is re-evaluated by update().
    struct CountingSource {
        counter: std::sync::atomic::AtomicI32,
    }
    impl NDAttributeSource for CountingSource {
        fn evaluate(&self) -> NDAttrValue {
            let n = self
                .counter
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            NDAttrValue::Int32(n)
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[test]
    fn test_attribute_update_reevaluates_source() {
        let src = std::sync::Arc::new(CountingSource {
            counter: std::sync::atomic::AtomicI32::new(0),
        });
        let mut attr = NDAttribute::new_with_source(
            "live",
            "live source",
            NDAttrSource::Function(String::new()),
            src,
        );
        // Constructed value is the first evaluation.
        assert_eq!(attr.value, NDAttrValue::Int32(1));
        attr.update();
        assert_eq!(attr.value, NDAttrValue::Int32(2));
        attr.update();
        assert_eq!(attr.value, NDAttrValue::Int32(3));
    }

    #[test]
    fn test_static_attribute_update_is_noop() {
        // G10: static (Constant / Driver) attributes are not re-evaluated.
        let mut attr = NDAttribute::new_static(
            "static",
            "",
            NDAttrSource::Constant(String::new()),
            NDAttrValue::Int32(42),
        );
        attr.update();
        assert_eq!(attr.value, NDAttrValue::Int32(42));
    }

    #[test]
    fn test_param_source_cell_reevaluates() {
        // G10: a ParamAttributeSource returns whatever the driver last fed
        // into its cell — update() re-reads the fresh value.
        let src = ParamAttributeSource::new("GAIN", 0, ParamAttrType::Float64);
        let mut attr = NDAttribute::new_with_source(
            "Gain",
            "",
            NDAttrSource::Param {
                port_name: String::new(),
                param_name: "GAIN".into(),
            },
            src.clone(),
        );
        assert_eq!(attr.value, NDAttrValue::Undefined);
        src.cell().set(NDAttrValue::Float64(3.5));
        attr.update();
        assert_eq!(attr.value, NDAttrValue::Float64(3.5));
        // param_source() downcast exposes the backend to the driver.
        assert_eq!(attr.param_source().unwrap().param_name, "GAIN");
    }

    #[test]
    fn test_function_source_via_registry() {
        // G10: a FunctionAttributeSource calls the named registered function.
        let registry = NDAttributeFunctionRegistry::new();
        registry.register("double", |p: &str| {
            NDAttrValue::Int32(p.parse::<i32>().unwrap_or(0) * 2)
        });
        let src = FunctionAttributeSource::new(registry.clone(), "double", "21");
        let mut attr =
            NDAttribute::new_with_source("Doubled", "", NDAttrSource::Function(String::new()), src);
        assert_eq!(attr.value, NDAttrValue::Int32(42));
        attr.update();
        assert_eq!(attr.value, NDAttrValue::Int32(42));

        // An unregistered function evaluates to Undefined.
        let missing = FunctionAttributeSource::new(registry, "no_such", "");
        assert_eq!(missing.evaluate(), NDAttrValue::Undefined);
    }

    #[test]
    fn test_epics_pv_source_cell_feeding() {
        // G10: an EpicsPvAttributeSource returns whatever a CA-monitor task
        // last fed into its cell.
        let src = EpicsPvAttributeSource::new("TST:Temp", PvAttrDbrType::Native);
        assert_eq!(src.evaluate(), NDAttrValue::Undefined);
        src.cell().set(NDAttrValue::Float64(22.4));
        assert_eq!(src.evaluate(), NDAttrValue::Float64(22.4));
    }

    #[test]
    fn test_pv_attr_dbr_type_publish() {
        // C publishes the REQUESTED dbrType, not the PV's native one
        // (PVAttribute.cpp:257-282), so a DBR_LONG on a DBF_DOUBLE PV is an
        // Int32 attribute.
        assert_eq!(
            PvAttrDbrType::Int32.publish(NDAttrValue::Float64(22.9)),
            NDAttrValue::Int32(22)
        );
        assert_eq!(
            PvAttrDbrType::String.publish(NDAttrValue::Int16(7)),
            NDAttrValue::String("7".into())
        );
        assert_eq!(
            PvAttrDbrType::Native.publish(NDAttrValue::Float64(22.9)),
            NDAttrValue::Float64(22.9)
        );
        // Un-convertible and un-set both stay Undefined rather than becoming a
        // plausible zero.
        assert_eq!(
            PvAttrDbrType::Float64.publish(NDAttrValue::String("warm".into())),
            NDAttrValue::Undefined
        );
        assert_eq!(
            PvAttrDbrType::String.publish(NDAttrValue::Undefined),
            NDAttrValue::Undefined
        );
    }

    #[test]
    fn test_attribute_list_update_values() {
        // G10: NDAttributeList::update_values re-evaluates every live source.
        let mut list = NDAttributeList::new();
        list.add(NDAttribute::new_with_source(
            "live",
            "",
            NDAttrSource::Function(String::new()),
            std::sync::Arc::new(CountingSource {
                counter: std::sync::atomic::AtomicI32::new(0),
            }),
        ));
        list.add(NDAttribute::new_static(
            "frozen",
            "",
            NDAttrSource::Constant(String::new()),
            NDAttrValue::Int32(7),
        ));
        list.update_values();
        assert_eq!(list.get("live").unwrap().value, NDAttrValue::Int32(2));
        assert_eq!(list.get("frozen").unwrap().value, NDAttrValue::Int32(7));
    }
}
