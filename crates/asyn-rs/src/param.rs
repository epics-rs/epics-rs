use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use crate::error::{AsynError, AsynResult, AsynStatus};

/// A single entry in an enumeration parameter's choice list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumEntry {
    pub string: String,
    pub value: i32,
    pub severity: u16,
}

/// UInt32Digital interrupt-reason selector — C parity for
/// `interruptReason` (`interfaces/asynUInt32Digital.h:25-27`).
///
/// `ZeroToOne` (rising) and `OneToZero` (falling) configure each
/// mask in isolation; `Both` overwrites them together on set and
/// returns `rising | falling` on get (matching
/// `asynPortDriver.cpp:480-535`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptReason {
    ZeroToOne,
    OneToZero,
    Both,
}

/// Parameter data types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    Int32,
    Int64,
    UInt64,
    Float64,
    Octet,
    UInt32Digital,
    Int8Array,
    Int16Array,
    Int32Array,
    Int64Array,
    UInt64Array,
    Float32Array,
    Float64Array,
    Enum,
    GenericPointer,
}

/// Parameter value. Arrays use `Arc<[T]>` for cheap cloning during interrupt broadcast.
/// [`Octet`](Self::Octet) is a byte string — C's `char *` — and holds any byte
/// a device sends, a lone `0x80..=0xff` or an embedded NUL included.
#[derive(Clone)]
pub enum ParamValue {
    Int32(i32),
    Int64(i64),
    /// Unsigned 64-bit integer (asyn upstream issue #231).
    UInt64(u64),
    Float64(f64),
    /// C `paramVal`'s `sval` — a `char *` the driver fills from the device.
    /// It is a byte string, not text: `doCallbacksOctet` hands the subscriber
    /// `val.c_str()` and a device answer may carry any byte, so a `String` here
    /// could not hold one (`0x80..=0xff` alone is not UTF-8) and replaced it
    /// with U+FFFD on the way in.
    Octet(Vec<u8>),
    UInt32Digital(u32),
    Int8Array(Arc<[i8]>),
    Int16Array(Arc<[i16]>),
    Int32Array(Arc<[i32]>),
    Int64Array(Arc<[i64]>),
    /// Unsigned 64-bit integer array (asyn upstream issue #231).
    UInt64Array(Arc<[u64]>),
    Float32Array(Arc<[f32]>),
    Float64Array(Arc<[f64]>),
    Enum {
        index: usize,
        choices: Arc<[EnumEntry]>,
    },
    GenericPointer(Arc<dyn Any + Send + Sync>),
    Undefined,
}

impl fmt::Debug for ParamValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int32(v) => write!(f, "Int32({v:?})"),
            Self::Int64(v) => write!(f, "Int64({v:?})"),
            Self::UInt64(v) => write!(f, "UInt64({v:?})"),
            Self::Float64(v) => write!(f, "Float64({v:?})"),
            // Printed as text, since that is what an octet parameter almost
            // always holds; a byte outside UTF-8 shows as U+FFFD here and is
            // untouched in the value itself.
            Self::Octet(v) => write!(f, "Octet({:?})", String::from_utf8_lossy(v)),
            Self::UInt32Digital(v) => write!(f, "UInt32Digital({v:?})"),
            Self::Int8Array(v) => write!(f, "Int8Array({v:?})"),
            Self::Int16Array(v) => write!(f, "Int16Array({v:?})"),
            Self::Int32Array(v) => write!(f, "Int32Array({v:?})"),
            Self::Int64Array(v) => write!(f, "Int64Array({v:?})"),
            Self::UInt64Array(v) => write!(f, "UInt64Array({v:?})"),
            Self::Float32Array(v) => write!(f, "Float32Array({v:?})"),
            Self::Float64Array(v) => write!(f, "Float64Array({v:?})"),
            Self::Enum { index, choices } => write!(f, "Enum(index={index}, choices={choices:?})"),
            Self::GenericPointer(v) => write!(f, "GenericPointer(<{:?}>)", (*v).type_id()),
            Self::Undefined => write!(f, "Undefined"),
        }
    }
}

impl ParamValue {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Int32(_) => "Int32",
            Self::Int64(_) => "Int64",
            Self::UInt64(_) => "UInt64",
            Self::Float64(_) => "Float64",
            Self::Octet(_) => "Octet",
            Self::UInt32Digital(_) => "UInt32Digital",
            Self::Int8Array(_) => "Int8Array",
            Self::Int16Array(_) => "Int16Array",
            Self::Int32Array(_) => "Int32Array",
            Self::Int64Array(_) => "Int64Array",
            Self::UInt64Array(_) => "UInt64Array",
            Self::Float32Array(_) => "Float32Array",
            Self::Float64Array(_) => "Float64Array",
            Self::Enum { .. } => "Enum",
            Self::GenericPointer(_) => "GenericPointer",
            Self::Undefined => "Undefined",
        }
    }

    /// Read this value through the **asynInt32** interface — the single owner
    /// of that interface's read rule, shared by the parameter-store getters
    /// ([`ParamList::get_int32`]) and the device-support I/O-Intr path, so a
    /// value delivered to a record cannot be typed differently from the same
    /// value read by a poll.
    ///
    /// A parameter whose type the interface cannot read is an
    /// [`AsynError::TypeMismatch`], never a coercion: C keeps one interrupt
    /// list per interface, so a record on `asynInt32` is only ever handed an
    /// int32 (an enum's index included — asynInt32 reads an enum's index
    /// transparently).
    pub fn as_int32(&self) -> AsynResult<i32> {
        match self {
            Self::Int32(v) => Ok(*v),
            Self::Enum { index, .. } => Ok(*index as i32),
            other => Err(AsynError::TypeMismatch {
                expected: "Int32",
                actual: other.type_name(),
            }),
        }
    }

    /// Read this value through the **asynInt64** interface. See [`Self::as_int32`].
    pub fn as_int64(&self) -> AsynResult<i64> {
        match self {
            Self::Int64(v) => Ok(*v),
            other => Err(AsynError::TypeMismatch {
                expected: "Int64",
                actual: other.type_name(),
            }),
        }
    }

    /// Read this value through the **asynFloat64** interface. See [`Self::as_int32`].
    pub fn as_float64(&self) -> AsynResult<f64> {
        match self {
            Self::Float64(v) => Ok(*v),
            other => Err(AsynError::TypeMismatch {
                expected: "Float64",
                actual: other.type_name(),
            }),
        }
    }

    /// Read this value through the **asynOctet** interface. See [`Self::as_int32`].
    pub fn as_octet(&self) -> AsynResult<&[u8]> {
        match self {
            Self::Octet(s) => Ok(s),
            other => Err(AsynError::TypeMismatch {
                expected: "Octet",
                actual: other.type_name(),
            }),
        }
    }

    /// Read this value through the **asynUInt32Digital** interface. See
    /// [`Self::as_int32`].
    pub fn as_uint32(&self) -> AsynResult<u32> {
        match self {
            Self::UInt32Digital(v) => Ok(*v),
            other => Err(AsynError::TypeMismatch {
                expected: "UInt32Digital",
                actual: other.type_name(),
            }),
        }
    }

    /// Read this value through the **asynEnum** interface. See [`Self::as_int32`].
    pub fn as_enum(&self) -> AsynResult<(usize, Arc<[EnumEntry]>)> {
        match self {
            Self::Enum { index, choices } => Ok((*index, choices.clone())),
            other => Err(AsynError::TypeMismatch {
                expected: "Enum",
                actual: other.type_name(),
            }),
        }
    }

    /// Whether this is an array or generic-pointer value. C asynPortDriver
    /// fires interrupts for these through the per-type doCallbacks*Array /
    /// doCallbacksGenericPointer paths, NOT `paramList::callCallbacks`
    /// (whose switch at asynPortDriver.cpp:846-865 handles only scalars).
    /// The callCallbacks isDefined gate (:845) therefore does not govern
    /// them, so the Rust flush must keep firing array triggers regardless
    /// of the `defined` flag.
    pub fn is_array(&self) -> bool {
        matches!(
            self,
            Self::Int8Array(_)
                | Self::Int16Array(_)
                | Self::Int32Array(_)
                | Self::Int64Array(_)
                | Self::UInt64Array(_)
                | Self::Float32Array(_)
                | Self::Float64Array(_)
                | Self::GenericPointer(_)
        )
    }
}

#[derive(Debug, Clone)]
struct ParamEntry {
    name: String,
    param_type: ParamType,
    value: ParamValue,
    /// Whether the value has been explicitly set (C parity: valueDefined).
    defined: bool,
    status: AsynStatus,
    alarm_status: u16,
    alarm_severity: u16,
    value_changed: bool,
    timestamp: Option<SystemTime>,
    /// UInt32Digital: bitmask of bits that changed (for interrupt filtering).
    /// C parity: uInt32CallbackMask in paramVal.
    uint32_interrupt_mask: u32,
    /// UInt32Digital: bits the driver wants to fire callbacks on when
    /// they transition `0 → 1`. C parity: `paramVal::uInt32RisingMask`
    /// (`asynPortDriver/paramVal.h:45`) — written by
    /// `paramList::setUInt32Interrupt(idx, mask, interruptOnZeroToOne)`
    /// (`asynPortDriver.cpp:480-497`), read by `report`/`getInterrupt`.
    uint32_rising_mask: u32,
    /// UInt32Digital: bits the driver wants to fire callbacks on when
    /// they transition `1 → 0`. C parity: `paramVal::uInt32FallingMask`
    /// (`asynPortDriver/paramVal.h:46`).
    uint32_falling_mask: u32,
}

impl ParamEntry {
    fn new(name: String, param_type: ParamType) -> Self {
        // C parity: parameters start with default values but are marked as
        // "not yet defined" (valueDefined=false in C). In C, getters still
        // return the default value but also return asynParamUndefined status.
        // In Rust, we keep the values accessible but track the defined flag.
        let value = match param_type {
            ParamType::Int32 => ParamValue::Int32(0),
            ParamType::Int64 => ParamValue::Int64(0),
            ParamType::UInt64 => ParamValue::UInt64(0),
            ParamType::Float64 => ParamValue::Float64(0.0),
            ParamType::Octet => ParamValue::Octet(Vec::new()),
            ParamType::UInt32Digital => ParamValue::UInt32Digital(0),
            ParamType::Int8Array => ParamValue::Int8Array(Arc::from([] as [i8; 0])),
            ParamType::Int16Array => ParamValue::Int16Array(Arc::from([] as [i16; 0])),
            ParamType::Int32Array => ParamValue::Int32Array(Arc::from([] as [i32; 0])),
            ParamType::Int64Array => ParamValue::Int64Array(Arc::from([] as [i64; 0])),
            ParamType::UInt64Array => ParamValue::UInt64Array(Arc::from([] as [u64; 0])),
            ParamType::Float32Array => ParamValue::Float32Array(Arc::from([] as [f32; 0])),
            ParamType::Float64Array => ParamValue::Float64Array(Arc::from([] as [f64; 0])),
            ParamType::Enum => ParamValue::Enum {
                index: 0,
                choices: Arc::from([EnumEntry {
                    string: String::new(),
                    value: 0,
                    severity: 0,
                }]),
            },
            ParamType::GenericPointer => ParamValue::GenericPointer(Arc::new(())),
        };
        Self {
            name,
            param_type,
            value,
            defined: false,
            status: AsynStatus::Success,
            alarm_status: 0,
            alarm_severity: 0,
            value_changed: false,
            timestamp: None,
            uint32_interrupt_mask: 0,
            uint32_rising_mask: 0,
            uint32_falling_mask: 0,
        }
    }
}

/// Parameter library managing named parameters indexed by integer.
/// Supports multiple addresses (addr 0..max_addr).
pub struct ParamList {
    max_addr: usize,
    /// `params[addr][index] = ParamEntry`
    params: Vec<Vec<ParamEntry>>,
    name_to_index: HashMap<String, usize>,
}

impl ParamList {
    pub fn new(max_addr: usize) -> Self {
        let max_addr = max_addr.max(1);
        Self {
            max_addr,
            params: (0..max_addr).map(|_| Vec::new()).collect(),
            name_to_index: HashMap::new(),
        }
    }

    /// C `asynPortDriver::getAddress` (asynPortDriver.cpp:1901-1913): `-1` is
    /// the asynManager sentinel for a user carrying no device, so it becomes
    /// list 0 *before* the range check, and the same check then runs whatever
    /// the port is. C reads no `ASYN_MULTIDEVICE` flag here — a single-device
    /// port is simply one with `maxAddr == 1`, so every other address fails
    /// the one test. Selecting the rule on the flag instead gave `-1` two
    /// meanings, and a statistics parameter read through a default
    /// [`crate::user::AsynUser`] was refused on exactly the multi-device ports C
    /// serves it on.
    fn validate_addr(&self, addr: i32) -> AsynResult<usize> {
        let addr = if addr == -1 { 0 } else { addr };
        if addr < 0 || (addr as usize) >= self.max_addr {
            return Err(AsynError::AddressOutOfRange(addr));
        }
        Ok(addr as usize)
    }

    fn get_entry(&self, index: usize, addr: i32) -> AsynResult<&ParamEntry> {
        let a = self.validate_addr(addr)?;
        self.params[a]
            .get(index)
            .ok_or(AsynError::ParamIndexOutOfRange(index))
    }

    fn get_entry_mut(&mut self, index: usize, addr: i32) -> AsynResult<&mut ParamEntry> {
        let a = self.validate_addr(addr)?;
        self.params[a]
            .get_mut(index)
            .ok_or(AsynError::ParamIndexOutOfRange(index))
    }

    /// Create a parameter. Returns its index.
    /// The parameter is created at all addresses.
    ///
    /// Lax variant — when `name` already exists this returns the
    /// existing index silently (`asynSuccess`). That matches the
    /// idempotent build pattern used by `ad-core-rs::ADDriverParams`
    /// and `ad-plugins-rs::NDArrayDriverParams` (the latter create the
    /// shared `ACQUIRE`/`ACQUIRE_BUSY`/`WAIT_FOR_PLUGINS` params, then
    /// `ADDriverParams::create` re-issues `create_param` on the same
    /// names).
    ///
    /// Use [`Self::create_param_strict`] when you need C parity for
    /// `asynParamAlreadyExists` — `paramList::createParam`
    /// (`asynPortDriver/asynPortDriver.cpp:126-138`) returns that
    /// status on duplicate and the wrapper at
    /// `asynPortDriver::createParam(int list, ...)` (lines 991-1011)
    /// translates it to `asynError` with an `ASYN_TRACE_ERROR` log.
    pub fn create_param(&mut self, name: &str, param_type: ParamType) -> AsynResult<usize> {
        if let Some(&idx) = self.name_to_index.get(name) {
            return Ok(idx);
        }
        self.append_param(name, param_type)
    }

    /// Strict variant of [`Self::create_param`] — surfaces
    /// [`AsynError::ParamAlreadyExists`] on duplicate, matching the C
    /// `asynParamAlreadyExists` status returned by
    /// `paramList::createParam` (`asynPortDriver.cpp:126-138`).
    ///
    /// Use this from new callers that should match the C semantics.
    /// Existing callers (`ad-core-rs`, `ad-plugins-rs`) keep using the
    /// lax [`Self::create_param`] until they migrate; switching them
    /// requires teaching the duplicate-name shared-base-class build
    /// to look up the existing index instead of re-issuing
    /// `create_param`.
    pub fn create_param_strict(&mut self, name: &str, param_type: ParamType) -> AsynResult<usize> {
        if self.name_to_index.contains_key(name) {
            return Err(AsynError::ParamAlreadyExists(name.to_string()));
        }
        self.append_param(name, param_type)
    }

    fn append_param(&mut self, name: &str, param_type: ParamType) -> AsynResult<usize> {
        let index = self.params[0].len();
        for addr_params in &mut self.params {
            addr_params.push(ParamEntry::new(name.to_string(), param_type));
        }
        self.name_to_index.insert(name.to_string(), index);
        Ok(index)
    }

    /// Find parameter index by name.
    pub fn find_param(&self, name: &str) -> Option<usize> {
        self.name_to_index.get(name).copied()
    }

    /// Get parameter name by index.
    pub fn param_name(&self, index: usize) -> Option<&str> {
        self.params[0].get(index).map(|e| e.name.as_str())
    }

    /// Get parameter type by index.
    pub fn param_type(&self, index: usize) -> Option<ParamType> {
        self.params[0].get(index).map(|e| e.param_type)
    }

    /// Get the raw ParamValue.
    pub fn get_value(&self, index: usize, addr: i32) -> AsynResult<&ParamValue> {
        Ok(&self.get_entry(index, addr)?.value)
    }

    // --- Scalar getters/setters ---

    /// Read the cached Int32 value, falling back to the type default
    /// (`0`) if the parameter has not been set.
    ///
    /// Use [`Self::get_int32_strict`] when you need C parity for
    /// `asynParamUndefined` — the lax variant matches the
    /// `.unwrap_or(0)` convention used throughout the workspace and
    /// stays callable on never-set params.
    pub fn get_int32(&self, index: usize, addr: i32) -> AsynResult<i32> {
        self.get_entry(index, addr)?.value.as_int32()
    }

    /// Strict variant of [`Self::get_int32`] — returns
    /// [`AsynError::ParamUndefined`] when the entry has never been set.
    ///
    /// C parity: `paramVal::getInteger` (`paramVal.cpp:147-154`) checks
    /// `WrongType` first then `NotDefined`; the wrapping
    /// `paramList::getInteger` (`asynPortDriver.cpp:301-321`) translates
    /// `ParamValNotDefined` to `asynParamUndefined`.
    pub fn get_int32_strict(&self, index: usize, addr: i32) -> AsynResult<i32> {
        let entry = self.get_entry(index, addr)?;
        // C order: WrongType first (`as_int32`), then NotDefined.
        let value = entry.value.as_int32()?;
        if !entry.defined {
            return Err(AsynError::ParamUndefined(index));
        }
        Ok(value)
    }

    pub fn set_int32(&mut self, index: usize, addr: i32, value: i32) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        match entry.value {
            ParamValue::Int32(ref old) => {
                // C parity: paramVal::setInteger gates on
                // `!isDefined() || (data.ival != value)` (paramVal.cpp:131-141);
                // a first set with `value == default` still flips `defined`.
                if !entry.defined || *old != value {
                    entry.value = ParamValue::Int32(value);
                    entry.value_changed = true;
                    entry.defined = true;
                }
            }
            // C EPICS asyn: asynInt32 interface writes enum index transparently
            ParamValue::Enum {
                ref choices,
                ref mut index,
            } => {
                let new_idx = value as usize;
                if new_idx >= choices.len() {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!(
                            "enum index {new_idx} out of range (0..{})",
                            choices.len()
                        ),
                    });
                }
                if !entry.defined || *index != new_idx {
                    *index = new_idx;
                    entry.value_changed = true;
                    entry.defined = true;
                }
            }
            _ => {
                return Err(AsynError::TypeMismatch {
                    expected: "Int32",
                    actual: entry.value.type_name(),
                });
            }
        }
        Ok(())
    }

    /// Lax variant — returns `0.0` for an undefined Float64. Use
    /// [`Self::get_float64_strict`] for C parity.
    pub fn get_float64(&self, index: usize, addr: i32) -> AsynResult<f64> {
        self.get_entry(index, addr)?.value.as_float64()
    }

    /// Strict variant — returns [`AsynError::ParamUndefined`] when the
    /// entry has never been set. C parity:
    /// `paramVal::getDouble` (`paramVal.cpp:259-266`) +
    /// `paramList::getDouble` (`asynPortDriver.cpp:383-403`).
    pub fn get_float64_strict(&self, index: usize, addr: i32) -> AsynResult<f64> {
        let entry = self.get_entry(index, addr)?;
        let value = entry.value.as_float64()?;
        if !entry.defined {
            return Err(AsynError::ParamUndefined(index));
        }
        Ok(value)
    }

    pub fn set_float64(&mut self, index: usize, addr: i32, value: f64) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        if let ParamValue::Float64(ref old) = entry.value {
            // C parity: paramVal::setDouble (paramVal.cpp:243-253) —
            // `!isDefined() || data.dval != value` flips defined on the
            // first set even when `value` equals the type default.
            if !entry.defined || *old != value {
                entry.value = ParamValue::Float64(value);
                entry.value_changed = true;
                entry.defined = true;
            }
        } else {
            return Err(AsynError::TypeMismatch {
                expected: "Float64",
                actual: entry.value.type_name(),
            });
        }
        Ok(())
    }

    /// Lax variant — returns `0` for an undefined Int64. Use
    /// [`Self::get_int64_strict`] for C parity.
    pub fn get_int64(&self, index: usize, addr: i32) -> AsynResult<i64> {
        self.get_entry(index, addr)?.value.as_int64()
    }

    /// Strict variant — returns [`AsynError::ParamUndefined`] when the
    /// entry has never been set. C parity: `paramVal::getInteger64`
    /// (`paramVal.cpp:176-183`) + `paramList::getInteger64`
    /// (`asynPortDriver.cpp:328-348`).
    pub fn get_int64_strict(&self, index: usize, addr: i32) -> AsynResult<i64> {
        let entry = self.get_entry(index, addr)?;
        let value = entry.value.as_int64()?;
        if !entry.defined {
            return Err(AsynError::ParamUndefined(index));
        }
        Ok(value)
    }

    pub fn set_int64(&mut self, index: usize, addr: i32, value: i64) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        if let ParamValue::Int64(ref old) = entry.value {
            // C parity: paramVal::setInteger64 (paramVal.cpp:160-170).
            if !entry.defined || *old != value {
                entry.value = ParamValue::Int64(value);
                entry.value_changed = true;
                entry.defined = true;
            }
        } else {
            return Err(AsynError::TypeMismatch {
                expected: "Int64",
                actual: entry.value.type_name(),
            });
        }
        Ok(())
    }

    /// Lax variant — returns no bytes for an undefined Octet.
    /// Use [`Self::get_string_strict`] for C parity.
    pub fn get_string(&self, index: usize, addr: i32) -> AsynResult<&[u8]> {
        self.get_entry(index, addr)?.value.as_octet()
    }

    /// Strict variant — returns [`AsynError::ParamUndefined`] when the
    /// entry has never been set. C parity: `paramVal::getString`
    /// (`paramVal.cpp:287-294`) + `paramList::getString`
    /// (`asynPortDriver.cpp:543-565`).
    pub fn get_string_strict(&self, index: usize, addr: i32) -> AsynResult<&[u8]> {
        let entry = self.get_entry(index, addr)?;
        let value = entry.value.as_octet()?;
        if !entry.defined {
            return Err(AsynError::ParamUndefined(index));
        }
        Ok(value)
    }

    /// `value` is taken as bytes — C's `setStringParam(int, const char *)`
    /// copies whatever the driver points at, and a device answer is not
    /// required to be UTF-8. `String`/`&str` callers are unaffected.
    pub fn set_string(
        &mut self,
        index: usize,
        addr: i32,
        value: impl Into<Vec<u8>>,
    ) -> AsynResult<()> {
        let value = value.into();
        let entry = self.get_entry_mut(index, addr)?;
        if let ParamValue::Octet(ref old) = entry.value {
            // C parity: paramVal::setString (paramVal.cpp:271-281) —
            // `!isDefined() || sval != value`.
            if !entry.defined || *old != value {
                entry.value = ParamValue::Octet(value);
                entry.value_changed = true;
                entry.defined = true;
            }
        } else {
            return Err(AsynError::TypeMismatch {
                expected: "Octet",
                actual: entry.value.type_name(),
            });
        }
        Ok(())
    }

    /// Lax variant — returns `0` for an undefined UInt32Digital. Use
    /// [`Self::get_uint32_strict`] for C parity.
    pub fn get_uint32(&self, index: usize, addr: i32) -> AsynResult<u32> {
        self.get_entry(index, addr)?.value.as_uint32()
    }

    /// Strict variant — returns [`AsynError::ParamUndefined`] when the
    /// entry has never been set. C parity: `paramVal::getUInt32`
    /// (`paramVal.cpp:230-237`) + `paramList::getUInt32`
    /// (`asynPortDriver.cpp:356-376`).
    pub fn get_uint32_strict(&self, index: usize, addr: i32) -> AsynResult<u32> {
        let entry = self.get_entry(index, addr)?;
        let value = entry.value.as_uint32()?;
        if !entry.defined {
            return Err(AsynError::ParamUndefined(index));
        }
        Ok(value)
    }

    pub fn set_uint32(
        &mut self,
        index: usize,
        addr: i32,
        value: u32,
        mask: u32,
        interrupt_mask: u32,
    ) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        if let ParamValue::UInt32Digital(ref old) = entry.value {
            // C parity: paramVal::setUInt32 (paramVal.cpp:197-225).
            //   if (!isDefined()) { uival = 0; setDefined(true); setValueChanged(); }
            //   newValue = (uival & ~mask) | (value & mask);
            //   if (uival != newValue) { callbackMask |= (uival ^ newValue); ... }
            // The first write must flip `defined` and mark `value_changed`
            // BEFORE any merge, even if `value & mask == 0` — otherwise
            // the equivalent of `setUIntDigitalParam(idx, 0, mask)` on a
            // fresh param leaves the entry undefined and never notifies.
            let was_defined = entry.defined;
            let starting = if was_defined { *old } else { 0 };
            let new_val = (starting & !mask) | (value & mask);
            let changed_bits = if was_defined {
                starting ^ new_val
            } else {
                new_val
            };
            if !was_defined || starting != new_val {
                // C parity: `uInt32CallbackMask |= (data.uival ^ newValue)`
                // (paramVal.cpp:215) — accumulate the union of changed bits
                // across every setUInt32 before one callParamCallbacks, not
                // just the last set's bits. The flush owner resets the mask
                // to 0 after firing (see take_uint32_interrupt_mask).
                entry.uint32_interrupt_mask |= changed_bits;
                entry.value = ParamValue::UInt32Digital(new_val);
                entry.value_changed = true;
                entry.defined = true;
            }
            // C parity: paramVal.cpp:220-224 — a non-zero `interruptMask`
            // forces those bits into the callback mask and marks the value
            // changed EVEN when the stored value is unchanged, so I/O Intr
            // callbacks gated on those bits still fire. This is the
            // `setUIntDigitalParam(.., interruptMask)` overload path; the
            // first-write `defined` flip above already ran when needed.
            if interrupt_mask != 0 {
                entry.uint32_interrupt_mask |= interrupt_mask;
                entry.value_changed = true;
            }
        } else {
            return Err(AsynError::TypeMismatch {
                expected: "UInt32Digital",
                actual: entry.value.type_name(),
            });
        }
        Ok(())
    }

    /// Get the UInt32Digital interrupt mask (accumulated changed bits).
    pub fn get_uint32_interrupt_mask(&self, index: usize, addr: i32) -> AsynResult<u32> {
        Ok(self.get_entry(index, addr)?.uint32_interrupt_mask)
    }

    /// Read the accumulated UInt32Digital callback mask AND reset it to 0.
    ///
    /// C parity: `uint32Callback` clears `uInt32CallbackMask = 0`
    /// immediately after firing (`asynPortDriver.cpp:855`). The flush is
    /// the single owner of this transition — it consumes the mask so the
    /// next flush starts clean and accumulated bits do not leak forward.
    /// A no-op returning 0 for non-UInt32 params, whose mask is always 0.
    pub fn take_uint32_interrupt_mask(&mut self, index: usize, addr: i32) -> AsynResult<u32> {
        let entry = self.get_entry_mut(index, addr)?;
        let mask = entry.uint32_interrupt_mask;
        entry.uint32_interrupt_mask = 0;
        Ok(mask)
    }

    /// Configure which bits of a UInt32Digital parameter should fire
    /// interrupts on transition.
    ///
    /// C parity: `paramList::setUInt32Interrupt`
    /// (`asynPortDriver/asynPortDriver.cpp:480-497`). The reason
    /// argument decides which mask (rising-only, falling-only, or
    /// both) is overwritten.
    pub fn set_uint32_interrupt(
        &mut self,
        index: usize,
        addr: i32,
        mask: u32,
        reason: InterruptReason,
    ) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        if entry.param_type != ParamType::UInt32Digital {
            return Err(AsynError::TypeMismatch {
                expected: "UInt32Digital",
                actual: entry.value.type_name(),
            });
        }
        match reason {
            InterruptReason::ZeroToOne => entry.uint32_rising_mask = mask,
            InterruptReason::OneToZero => entry.uint32_falling_mask = mask,
            InterruptReason::Both => {
                entry.uint32_rising_mask = mask;
                entry.uint32_falling_mask = mask;
            }
        }
        Ok(())
    }

    /// Clear bits from BOTH rising and falling masks for a
    /// UInt32Digital parameter — C parity:
    /// `paramList::clearUInt32Interrupt`
    /// (`asynPortDriver.cpp:504-511`). The C function does not
    /// accept an `interruptReason`; rising and falling masks are
    /// always cleared together.
    pub fn clear_uint32_interrupt(&mut self, index: usize, addr: i32, mask: u32) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        if entry.param_type != ParamType::UInt32Digital {
            return Err(AsynError::TypeMismatch {
                expected: "UInt32Digital",
                actual: entry.value.type_name(),
            });
        }
        entry.uint32_rising_mask &= !mask;
        entry.uint32_falling_mask &= !mask;
        Ok(())
    }

    /// Read the configured rising / falling / combined mask.
    ///
    /// C parity: `paramList::getUInt32Interrupt`
    /// (`asynPortDriver.cpp:519-535`). For `Both`, the combined mask
    /// is `rising | falling` (matching the C semantics).
    pub fn get_uint32_interrupt(
        &self,
        index: usize,
        addr: i32,
        reason: InterruptReason,
    ) -> AsynResult<u32> {
        let entry = self.get_entry(index, addr)?;
        if entry.param_type != ParamType::UInt32Digital {
            return Err(AsynError::TypeMismatch {
                expected: "UInt32Digital",
                actual: entry.value.type_name(),
            });
        }
        Ok(match reason {
            InterruptReason::ZeroToOne => entry.uint32_rising_mask,
            InterruptReason::OneToZero => entry.uint32_falling_mask,
            InterruptReason::Both => entry.uint32_rising_mask | entry.uint32_falling_mask,
        })
    }

    // --- Array getters/setters ---

    pub fn get_float64_array(&self, index: usize, addr: i32) -> AsynResult<Arc<[f64]>> {
        match &self.get_entry(index, addr)?.value {
            ParamValue::Float64Array(v) => Ok(v.clone()),
            other => Err(AsynError::TypeMismatch {
                expected: "Float64Array",
                actual: other.type_name(),
            }),
        }
    }

    pub fn set_float64_array(&mut self, index: usize, addr: i32, data: Vec<f64>) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        if matches!(entry.value, ParamValue::Float64Array(_)) {
            entry.value = ParamValue::Float64Array(Arc::from(data));
            entry.value_changed = true;
            entry.defined = true;
            Ok(())
        } else {
            Err(AsynError::TypeMismatch {
                expected: "Float64Array",
                actual: entry.value.type_name(),
            })
        }
    }

    pub fn get_int32_array(&self, index: usize, addr: i32) -> AsynResult<Arc<[i32]>> {
        match &self.get_entry(index, addr)?.value {
            ParamValue::Int32Array(v) => Ok(v.clone()),
            other => Err(AsynError::TypeMismatch {
                expected: "Int32Array",
                actual: other.type_name(),
            }),
        }
    }

    pub fn set_int32_array(&mut self, index: usize, addr: i32, data: Vec<i32>) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        if matches!(entry.value, ParamValue::Int32Array(_)) {
            entry.value = ParamValue::Int32Array(Arc::from(data));
            entry.value_changed = true;
            entry.defined = true;
            Ok(())
        } else {
            Err(AsynError::TypeMismatch {
                expected: "Int32Array",
                actual: entry.value.type_name(),
            })
        }
    }

    pub fn get_int8_array(&self, index: usize, addr: i32) -> AsynResult<Arc<[i8]>> {
        match &self.get_entry(index, addr)?.value {
            ParamValue::Int8Array(v) => Ok(v.clone()),
            other => Err(AsynError::TypeMismatch {
                expected: "Int8Array",
                actual: other.type_name(),
            }),
        }
    }

    pub fn set_int8_array(&mut self, index: usize, addr: i32, data: Vec<i8>) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        if matches!(entry.value, ParamValue::Int8Array(_)) {
            entry.value = ParamValue::Int8Array(Arc::from(data));
            entry.value_changed = true;
            entry.defined = true;
            Ok(())
        } else {
            Err(AsynError::TypeMismatch {
                expected: "Int8Array",
                actual: entry.value.type_name(),
            })
        }
    }

    pub fn get_int16_array(&self, index: usize, addr: i32) -> AsynResult<Arc<[i16]>> {
        match &self.get_entry(index, addr)?.value {
            ParamValue::Int16Array(v) => Ok(v.clone()),
            other => Err(AsynError::TypeMismatch {
                expected: "Int16Array",
                actual: other.type_name(),
            }),
        }
    }

    pub fn set_int16_array(&mut self, index: usize, addr: i32, data: Vec<i16>) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        if matches!(entry.value, ParamValue::Int16Array(_)) {
            entry.value = ParamValue::Int16Array(Arc::from(data));
            entry.value_changed = true;
            entry.defined = true;
            Ok(())
        } else {
            Err(AsynError::TypeMismatch {
                expected: "Int16Array",
                actual: entry.value.type_name(),
            })
        }
    }

    pub fn get_int64_array(&self, index: usize, addr: i32) -> AsynResult<Arc<[i64]>> {
        match &self.get_entry(index, addr)?.value {
            ParamValue::Int64Array(v) => Ok(v.clone()),
            other => Err(AsynError::TypeMismatch {
                expected: "Int64Array",
                actual: other.type_name(),
            }),
        }
    }

    pub fn set_int64_array(&mut self, index: usize, addr: i32, data: Vec<i64>) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        if matches!(entry.value, ParamValue::Int64Array(_)) {
            entry.value = ParamValue::Int64Array(Arc::from(data));
            entry.value_changed = true;
            entry.defined = true;
            Ok(())
        } else {
            Err(AsynError::TypeMismatch {
                expected: "Int64Array",
                actual: entry.value.type_name(),
            })
        }
    }

    pub fn get_float32_array(&self, index: usize, addr: i32) -> AsynResult<Arc<[f32]>> {
        match &self.get_entry(index, addr)?.value {
            ParamValue::Float32Array(v) => Ok(v.clone()),
            other => Err(AsynError::TypeMismatch {
                expected: "Float32Array",
                actual: other.type_name(),
            }),
        }
    }

    pub fn set_float32_array(&mut self, index: usize, addr: i32, data: Vec<f32>) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        if matches!(entry.value, ParamValue::Float32Array(_)) {
            entry.value = ParamValue::Float32Array(Arc::from(data));
            entry.value_changed = true;
            entry.defined = true;
            Ok(())
        } else {
            Err(AsynError::TypeMismatch {
                expected: "Float32Array",
                actual: entry.value.type_name(),
            })
        }
    }

    // --- Enum getters/setters ---

    pub fn get_enum(&self, index: usize, addr: i32) -> AsynResult<(usize, Arc<[EnumEntry]>)> {
        self.get_entry(index, addr)?.value.as_enum()
    }

    pub fn set_enum_index(&mut self, index: usize, addr: i32, value: usize) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        if let ParamValue::Enum {
            ref choices,
            index: ref mut idx,
        } = entry.value
        {
            if value >= choices.len() {
                return Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("enum index {value} out of range (0..{})", choices.len()),
                });
            }
            // C parity: enum index is stored through paramVal::setInteger,
            // which gates on `!isDefined() || data != value` — a first set
            // to index 0 (the default) must still flip `defined`.
            if !entry.defined || *idx != value {
                *idx = value;
                entry.value_changed = true;
                entry.defined = true;
            }
        } else {
            return Err(AsynError::TypeMismatch {
                expected: "Enum",
                actual: entry.value.type_name(),
            });
        }
        Ok(())
    }

    pub fn set_enum_choices(
        &mut self,
        index: usize,
        addr: i32,
        choices: Arc<[EnumEntry]>,
    ) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        if let ParamValue::Enum {
            index: ref mut idx,
            choices: ref mut ch,
        } = entry.value
        {
            *ch = choices;
            // Reset index if out of range
            if *idx >= ch.len() {
                *idx = 0;
            }
            entry.value_changed = true;
            entry.defined = true;
        } else {
            return Err(AsynError::TypeMismatch {
                expected: "Enum",
                actual: entry.value.type_name(),
            });
        }
        Ok(())
    }

    // --- GenericPointer getters/setters ---

    pub fn get_generic_pointer(
        &self,
        index: usize,
        addr: i32,
    ) -> AsynResult<Arc<dyn Any + Send + Sync>> {
        match &self.get_entry(index, addr)?.value {
            ParamValue::GenericPointer(v) => Ok(v.clone()),
            other => Err(AsynError::TypeMismatch {
                expected: "GenericPointer",
                actual: other.type_name(),
            }),
        }
    }

    pub fn set_generic_pointer(
        &mut self,
        index: usize,
        addr: i32,
        value: Arc<dyn Any + Send + Sync>,
    ) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        if matches!(entry.value, ParamValue::GenericPointer(_)) {
            entry.value = ParamValue::GenericPointer(value);
            entry.value_changed = true;
            entry.defined = true; // Any is not comparable
            Ok(())
        } else {
            Err(AsynError::TypeMismatch {
                expected: "GenericPointer",
                actual: entry.value.type_name(),
            })
        }
    }

    /// Store a value of **any** parameter type — the single owner of the
    /// "set a parameter from a `ParamValue`" dispatch.
    ///
    /// Every caller that holds a value whose type is only known at runtime (the
    /// port actor applying a [`crate::request::ParamSetValue`], above all) goes
    /// through this instead of re-enumerating the type set at the call site. The
    /// match is exhaustive, so a new [`ParamValue`] variant is a compile error
    /// here rather than a value that silently never reaches the store.
    ///
    /// `UInt32Digital` is stored with a full write mask and no forced interrupt
    /// bits; use [`Self::set_uint32`] directly for C `setUIntDigitalParam`'s
    /// masked set.
    pub fn set_value(&mut self, index: usize, addr: i32, value: ParamValue) -> AsynResult<()> {
        let type_name = value.type_name();
        match value {
            ParamValue::Int32(v) => self.set_int32(index, addr, v),
            ParamValue::Int64(v) => self.set_int64(index, addr, v),
            ParamValue::Float64(v) => self.set_float64(index, addr, v),
            ParamValue::Octet(s) => self.set_string(index, addr, s),
            ParamValue::UInt32Digital(v) => self.set_uint32(index, addr, v, u32::MAX, 0),
            ParamValue::Int8Array(a) => self.set_int8_array(index, addr, a.to_vec()),
            ParamValue::Int16Array(a) => self.set_int16_array(index, addr, a.to_vec()),
            ParamValue::Int32Array(a) => self.set_int32_array(index, addr, a.to_vec()),
            ParamValue::Int64Array(a) => self.set_int64_array(index, addr, a.to_vec()),
            ParamValue::Float32Array(a) => self.set_float32_array(index, addr, a.to_vec()),
            ParamValue::Float64Array(a) => self.set_float64_array(index, addr, a.to_vec()),
            ParamValue::Enum {
                index: idx,
                choices,
            } => {
                // Choices first — `set_enum_choices` clamps the index to the new
                // table, so setting the index afterwards is what sticks. An empty
                // table means "index only", leaving the parameter's own choices.
                if !choices.is_empty() {
                    self.set_enum_choices(index, addr, choices)?;
                }
                self.set_enum_index(index, addr, idx)
            }
            ParamValue::GenericPointer(p) => self.set_generic_pointer(index, addr, p),
            // `UInt64`/`UInt64Array` are declared in `ParamValue`/`ParamType` but
            // the store has no setter *or* getter for them (they are a Rust
            // extension with no upstream asyn interface — see
            // `interfaces::InterfaceType::UInt64`), so there is no store state a
            // caller could reach through them. Refuse loudly rather than pretend
            // the value landed.
            ParamValue::UInt64(_) | ParamValue::UInt64Array(_) => Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("{type_name} parameters have no store accessor; nothing to set"),
            }),
            ParamValue::Undefined => Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "cannot set a parameter to Undefined".into(),
            }),
        }
    }

    /// Check if a parameter has been explicitly set (C parity: valueDefined).
    pub fn is_param_defined(&self, index: usize, addr: i32) -> AsynResult<bool> {
        Ok(self.get_entry(index, addr)?.defined)
    }

    // --- Status ---

    pub fn set_param_status(
        &mut self,
        index: usize,
        addr: i32,
        status: AsynStatus,
        alarm_status: u16,
        alarm_severity: u16,
    ) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        // C paramVal::setStatus / setAlarmStatus / setAlarmSeverity
        // (paramVal.cpp:71-78,84-91,97-104) each call setValueChanged()
        // when their field actually changes; the paramList wrappers
        // (asynPortDriver.cpp:420-472) then run registerParameterChange ->
        // setFlag, marking the param dirty. So a STATUS- or ALARM-only
        // transition (no value change) is still delivered on the next
        // callParamCallbacks. Each changed setter also forces the full
        // UInt32Digital callback mask (0xFFFFFFFF) so every subscriber bit
        // fires. Pre-fix Rust assigned the fields without marking
        // value_changed, so a status/alarm-only transition was silently
        // dropped by take_changed (the standard setParamStatus() +
        // callParamCallbacks() alarm-push pattern never fired).
        let changed = entry.status != status
            || entry.alarm_status != alarm_status
            || entry.alarm_severity != alarm_severity;
        entry.status = status;
        entry.alarm_status = alarm_status;
        entry.alarm_severity = alarm_severity;
        if changed {
            entry.value_changed = true;
            if entry.param_type == ParamType::UInt32Digital {
                entry.uint32_interrupt_mask = 0xFFFF_FFFF;
            }
        }
        Ok(())
    }

    pub fn get_param_status(&self, index: usize, addr: i32) -> AsynResult<(AsynStatus, u16, u16)> {
        let entry = self.get_entry(index, addr)?;
        Ok((entry.status, entry.alarm_status, entry.alarm_severity))
    }

    // --- Timestamp ---

    pub fn set_timestamp(&mut self, index: usize, addr: i32, ts: SystemTime) -> AsynResult<()> {
        self.get_entry_mut(index, addr)?.timestamp = Some(ts);
        Ok(())
    }

    pub fn get_timestamp(&self, index: usize, addr: i32) -> AsynResult<Option<SystemTime>> {
        Ok(self.get_entry(index, addr)?.timestamp)
    }

    // --- Change tracking ---

    /// Clear the changed flag for a single parameter. Returns true if it was changed.
    pub fn take_changed_single(&mut self, index: usize, addr: i32) -> AsynResult<bool> {
        let entry = self.get_entry_mut(index, addr)?;
        let was_changed = entry.value_changed;
        entry.value_changed = false;
        Ok(was_changed)
    }

    /// Mark a parameter as changed without modifying its value.
    /// Useful for triggering I/O Intr on params whose data is served via
    /// read_*_array overrides rather than the param cache.
    pub fn mark_changed(&mut self, index: usize, addr: i32) -> AsynResult<()> {
        self.get_entry_mut(index, addr)?.value_changed = true;
        Ok(())
    }

    /// Returns indices of parameters whose values changed since last call, then clears flags.
    pub fn take_changed(&mut self, addr: i32) -> AsynResult<Vec<usize>> {
        let a = self.validate_addr(addr)?;
        let mut changed = Vec::new();
        for (i, entry) in self.params[a].iter_mut().enumerate() {
            if entry.value_changed {
                entry.value_changed = false;
                changed.push(i);
            }
        }
        Ok(changed)
    }

    /// Number of parameters.
    pub fn len(&self) -> usize {
        self.params[0].len()
    }

    pub fn is_empty(&self) -> bool {
        self.params[0].is_empty()
    }

    /// C `paramList::report` (asynPortDriver.cpp:885-892) — the count line, then
    /// every parameter through `ParamEntry::report`.
    ///
    /// C has one `paramList` per address and reports the one it was handed; here
    /// one list holds every address, so the address is the argument. It takes no
    /// detail level because C's does not use it: `paramVal::report` prints the
    /// same line whatever `details` says (paramVal.cpp:296-372), and the only
    /// thing the level decides is *how many* lists are printed — which is
    /// [`crate::port::PortDriverBase::report_params`]'s decision, and stays there.
    pub fn report(&self, out: &mut dyn std::fmt::Write, addr: i32) {
        use std::fmt::Write as _;
        let _ = writeln!(out, "Number of parameters is: {}", self.len());
        // A port whose `max_addr` outruns its list count (single-device, so every
        // addr collapses onto list 0) reports list 0 again, exactly as C does with
        // its `maxAddr` copies of the same values.
        let a = self.validate_addr(addr).unwrap_or(0);
        for (i, entry) in self.params[a].iter().enumerate() {
            entry.report(out, i);
        }
    }
}

impl ParamEntry {
    /// C `paramVal::report` (paramVal.cpp:296-372) — one line per parameter,
    /// name + type + value + status, *whatever* the detail level. C passes
    /// `details` in and never reads it; the only branch is defined vs undefined.
    fn report(&self, out: &mut dyn std::fmt::Write, id: usize) {
        use std::fmt::Write as _;
        let name = &self.name;
        // C's `asynStatus` is an unadorned enum and `%d` prints its ordinal;
        // [`AsynStatus`] declares the same six in the same order (asynDriver.h:48).
        let status = self.status as i32;

        // C's `switch(type)` has no case for `asynParamGenericPointer`, so it
        // lands in `default:` (paramVal.cpp:368-370) — a pointer parameter has no
        // printable value, and C says so rather than inventing one.
        if self.param_type == ParamType::GenericPointer {
            let _ = writeln!(out, "Parameter {id} is undefined, name={name}");
            return;
        }
        let type_name = c_param_type_name(self.param_type);
        if !self.defined {
            let _ = writeln!(
                out,
                "Parameter {id} type={type_name}, name={name}, value is undefined"
            );
            return;
        }
        match &self.value {
            ParamValue::Int32(v) => {
                let _ = writeln!(
                    out,
                    "Parameter {id} type={type_name}, name={name}, value={v}, status={status}"
                );
            }
            ParamValue::Int64(v) => {
                let _ = writeln!(
                    out,
                    "Parameter {id} type={type_name}, name={name}, value={v}, status={status}"
                );
            }
            ParamValue::UInt64(v) => {
                let _ = writeln!(
                    out,
                    "Parameter {id} type={type_name}, name={name}, value={v}, status={status}"
                );
            }
            // C prints the three masks alongside the value (paramVal.cpp:314-316):
            // a UInt32Digital parameter's callback behaviour is half its state.
            ParamValue::UInt32Digital(v) => {
                let _ = writeln!(
                    out,
                    "Parameter {id} type={type_name}, name={name}, value=0x{v:x}, \
                     status={status}, risingMask=0x{:x}, fallingMask=0x{:x}, callbackMask=0x{:x}",
                    self.uint32_rising_mask, self.uint32_falling_mask, self.uint32_interrupt_mask
                );
            }
            ParamValue::Float64(v) => {
                let _ = writeln!(
                    out,
                    "Parameter {id} type={type_name}, name={name}, value={}, status={status}",
                    format_g(*v)
                );
            }
            ParamValue::Octet(v) => {
                // C prints it with `%s` (asynPortDriver.cpp:2159) — the bytes as
                // text, stopping at nothing this port stores.
                let v = String::from_utf8_lossy(v);
                let _ = writeln!(
                    out,
                    "Parameter {id} type={type_name}, name={name}, value={v}, status={status}"
                );
            }
            // C prints the array's data pointer (`%p` over `data.pi8` &c.), not its
            // contents — the report says *that* there is an array and where, and
            // leaves the elements to the record that reads them.
            ParamValue::Int8Array(v) => report_array(out, id, type_name, name, v.as_ptr(), status),
            ParamValue::Int16Array(v) => report_array(out, id, type_name, name, v.as_ptr(), status),
            ParamValue::Int32Array(v) => report_array(out, id, type_name, name, v.as_ptr(), status),
            ParamValue::Int64Array(v) => report_array(out, id, type_name, name, v.as_ptr(), status),
            ParamValue::UInt64Array(v) => {
                report_array(out, id, type_name, name, v.as_ptr(), status)
            }
            ParamValue::Float32Array(v) => {
                report_array(out, id, type_name, name, v.as_ptr(), status)
            }
            ParamValue::Float64Array(v) => {
                report_array(out, id, type_name, name, v.as_ptr(), status)
            }
            ParamValue::Enum { index, .. } => {
                let _ = writeln!(
                    out,
                    "Parameter {id} type={type_name}, name={name}, value={index}, status={status}"
                );
            }
            // C's `asynParamNotDefined` — the type itself is unset, so the
            // `switch` falls to the same `default:` a generic pointer does
            // (paramVal.cpp:368-370).
            ParamValue::Undefined | ParamValue::GenericPointer(_) => {
                let _ = writeln!(out, "Parameter {id} is undefined, name={name}");
            }
        }
    }
}

fn report_array<T>(
    out: &mut dyn std::fmt::Write,
    id: usize,
    type_name: &str,
    name: &str,
    ptr: *const T,
    status: i32,
) {
    use std::fmt::Write as _;
    let _ = writeln!(
        out,
        "Parameter {id} type={type_name}, name={name}, value={ptr:p}, status={status}"
    );
}

/// The type name C's `paramVal::report` prints — note it is *not*
/// `paramVal::typeNames` (`asynParamInt32`, …): the report writes the interface
/// name (`asynInt32`), and calls octet `string` (paramVal.cpp:302-330).
///
/// `UInt64`, `UInt64Array` and `Enum` have no C `paramVal` case — they are this
/// port's extensions (upstream asyn issue #231 for the 64-bit unsigned pair) — so
/// they take the name C would have given them in the same scheme.
fn c_param_type_name(t: ParamType) -> &'static str {
    match t {
        ParamType::Int32 => "asynInt32",
        ParamType::Int64 => "asynInt64",
        ParamType::UInt64 => "asynUInt64",
        ParamType::Float64 => "asynFloat64",
        ParamType::Octet => "string",
        ParamType::UInt32Digital => "asynUInt32Digital",
        ParamType::Int8Array => "asynInt8Array",
        ParamType::Int16Array => "asynInt16Array",
        ParamType::Int32Array => "asynInt32Array",
        ParamType::Int64Array => "asynInt64Array",
        ParamType::UInt64Array => "asynUInt64Array",
        ParamType::Float32Array => "asynFloat32Array",
        ParamType::Float64Array => "asynFloat64Array",
        ParamType::Enum => "asynEnum",
        ParamType::GenericPointer => "asynGenericPointer",
    }
}

/// C `printf("%g")` over a double, which is what `paramVal::report` prints a
/// `Float64` with (paramVal.cpp:322): six significant digits, `%e` form when the
/// exponent is below -4 or at least 6, and trailing zeros stripped either way.
/// Rust's `{}` prints the shortest round-trip form instead — `1e-7` as
/// `0.0000001`, `0.1+0.2` as `0.30000000000000004` — so the line has to be built.
fn format_g(v: f64) -> String {
    if v.is_nan() {
        // glibc carries the NaN sign bit into the text, so C's
        // `printf("%g", -nan)` is `-nan`. Pinned by
        // [`libc_g_differential`].
        return if v.is_sign_negative() { "-nan" } else { "nan" }.to_string();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-inf" } else { "inf" }.to_string();
    }
    const PRECISION: i32 = 6;
    // Round to six significant digits first, and read the decimal exponent back
    // off the result: it is the rounded exponent that picks the form (9.999995
    // rounds to 1e+01, and C prints `10`, not `9.99999`).
    let sci = format!("{:.*e}", (PRECISION - 1) as usize, v);
    let (mantissa, exp) = sci.split_once('e').expect("Rust {:e} always emits one");
    let exp: i32 = exp.parse().expect("…followed by a decimal exponent");
    let strip = |s: &str| -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s.to_string()
        }
    };
    if exp < -4 || exp >= PRECISION {
        // C's `%e` exponent is signed and at least two digits: `1.5e+10`, `1e-05`.
        format!(
            "{}e{}{:02}",
            strip(mantissa),
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        )
    } else {
        strip(&format!("{:.*}", (PRECISION - 1 - exp).max(0) as usize, v))
    }
}

/// Flat parameter-group helper — C++ `asynParamSet` equivalent.
///
/// C asyn (`asynPortDriver/asynParamSet.h`) defines:
///
/// ```cpp
/// struct asynParam {
///     const char* name;
///     asynParamType type;
///     int* index;
/// };
/// class asynParamSet {
/// protected:
///     void add(const char* name, asynParamType type, int* index);
/// public:
///     std::vector<asynParam> getParamDefinitions();
/// };
/// ```
///
/// and `asynPortDriver::createParams` (asynPortDriver.cpp:4116-4126)
/// iterates the vector calling `createParam(name, type, index)` so the
/// driver subclass gets its int member back-filled.
///
/// In Rust we cannot stash a `&mut i32` for the duration of the build,
/// so the idiomatic equivalent is "register names + types, then run
/// `create_all` and read back the assigned indices as a Vec keyed by
/// add-order." Callers persist the returned `Vec<usize>` (or wrap it
/// in a domain-specific struct) the same way C++ drivers persist
/// their `int paramIdx` members.
///
/// The structure is intentionally a **flat list** — C asyn has no
/// tree / topology layer for parameter groups; a PVI-style topology
/// layer would be invention rather than compatibility.
#[derive(Default, Debug, Clone)]
pub struct AsynParamSet {
    defs: Vec<(String, ParamType)>,
}

impl AsynParamSet {
    pub fn new() -> Self {
        Self { defs: Vec::new() }
    }

    /// Append a parameter definition. Mirrors C++ `asynParamSet::add`.
    /// Returns the slot index — the position the param will occupy in
    /// the `Vec<usize>` returned by [`Self::create_all`].
    pub fn add(&mut self, name: &str, ty: ParamType) -> usize {
        let slot = self.defs.len();
        self.defs.push((name.to_string(), ty));
        slot
    }

    /// C++ `asynPortDriver::createParams` (asynPortDriver.cpp:4116-4126):
    /// iterate the definitions in registration order, call
    /// `createParam` for each, abort with `asynError` on the first
    /// failure. Returns the assigned indices in the same order as
    /// [`Self::add`] calls so the caller can recover slot→index
    /// without name lookup.
    pub fn create_all(&self, params: &mut ParamList) -> AsynResult<Vec<usize>> {
        let mut indices = Vec::with_capacity(self.defs.len());
        for (name, ty) in &self.defs {
            indices.push(params.create_param(name, *ty)?);
        }
        Ok(indices)
    }

    /// Number of definitions registered. Cheap inspector for tests.
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// Iterate definitions in registration order — read-only view.
    pub fn iter(&self) -> impl Iterator<Item = (&str, ParamType)> {
        self.defs.iter().map(|(n, t)| (n.as_str(), *t))
    }
}

/// The differential that makes three transcriptions of `%g` safe.
///
/// `epics-base-rs` (`printf` record), `epics-pva-rs` (`pvget`/`pvinfo`/
/// `pvmonitor`) and `asyn-rs` (`paramVal::report`) each carry their own
/// `format_g`, because sharing one would force a crate dependency none of
/// the three otherwise needs. What keeps them equal is not review but this
/// test: each crate runs the SAME sample through its own `format_g` and
/// through glibc's `snprintf("%.*g")` and requires byte equality. A
/// transcription that drifts fails here, in its own crate, on the sample
/// that caught the drift.
///
/// Gated on `target_env = "gnu"`: the assertion is glibc's exact output,
/// and newlib (RTEMS) / musl are not that reference.
#[cfg(all(test, unix, target_env = "gnu"))]
mod libc_g_differential {
    use super::format_g;

    /// glibc `printf("%.*g", prec, v)`.
    pub(super) fn libc_g(v: f64, prec: usize) -> String {
        let mut buf = [0u8; 512];
        // SAFETY: `buf` is 512 bytes and `snprintf` is given that length,
        // so it always NUL-terminates within bounds. `%.*g` of an f64 with
        // precision <= 17 never needs more than ~330 bytes.
        let n = unsafe {
            libc::snprintf(
                buf.as_mut_ptr().cast(),
                buf.len(),
                c"%.*g".as_ptr(),
                prec as libc::c_int,
                v,
            )
        };
        assert!(n >= 0 && (n as usize) < buf.len(), "snprintf overflow");
        String::from_utf8(buf[..n as usize].to_vec()).expect("glibc writes ASCII")
    }

    /// xorshift64*, so the sample is identical on every run and every host
    /// without pulling in a PRNG crate.
    pub(super) struct XorShift64(u64);
    impl XorShift64 {
        pub(super) fn new(seed: u64) -> Self {
            Self(seed)
        }
        pub(super) fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    /// Raw bit patterns (so NaN, infinities and subnormals arrive on their
    /// own), a decade sweep across the whole exponent range, and the
    /// boundary values `%g`'s style decision turns on.
    pub(super) fn sample() -> Vec<f64> {
        let mut out = vec![
            0.0,
            -0.0,
            f64::NAN,
            -f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
            f64::MAX,
            f64::MIN,
            1.0 / 3.0,
            0.1 + 0.2,
            // the rounded-exponent boundary: 9.999995 must print as C does
            9.999_995,
            99_999.5,
            999_999.5,
            0.000_099_999_95,
        ];
        for e in -320i32..=308 {
            for m in [1.0_f64, 1.5, 3.3333333, 9.999999, 9.9999995] {
                let v = m * 10f64.powi(e);
                if v.is_finite() {
                    out.push(v);
                    out.push(-v);
                }
            }
        }
        let mut rng = XorShift64::new(0x5EED_1234_ABCD_0001);
        for _ in 0..50_000 {
            out.push(f64::from_bits(rng.next()));
        }
        out
    }

    #[test]
    fn format_g_is_byte_identical_to_glibc() {
        let values = sample();
        let mut checked = 0usize;
        let mut mismatches: Vec<String> = Vec::new();
        // `paramVal::report` prints with plain `%g`, i.e. precision 6.
        for prec in [6usize] {
            for &v in &values {
                let ours = format_g(v);
                let theirs = libc_g(v, prec);
                checked += 1;
                if ours != theirs && mismatches.len() < 20 {
                    mismatches.push(format!(
                        "%.{prec}g of {v:?} (bits {:#018x}): ours {ours:?} != glibc {theirs:?}",
                        v.to_bits()
                    ));
                }
            }
        }
        assert!(
            mismatches.is_empty(),
            "{} of {checked} samples disagree with glibc:\n{}",
            mismatches.len(),
            mismatches.join("\n")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C `%g` (paramVal.cpp:322 prints a Float64 with it): six significant
    /// digits, `%e` form when the decimal exponent is < -4 or >= 6, trailing
    /// zeros stripped, two-digit signed exponent. One case per boundary of that
    /// rule — Rust's own `{}` matches none of them.
    #[test]
    fn format_g_matches_c_printf_g() {
        for (v, expect) in [
            (0.0f64, "0"),
            (7.0, "7"),
            (0.1 + 0.2, "0.3"),
            (1.5, "1.5"),
            // Six significant digits, then the trailing zeros go.
            (1.0 / 3.0, "0.333333"),
            (123456.0, "123456"),
            // exp == 6 → the %e form starts here.
            (1234567.0, "1.23457e+06"),
            // exp == -4 is still the %f form; -5 is not.
            (0.000123456789, "0.000123457"),
            (0.0000123456789, "1.23457e-05"),
            (-2.5e-9, "-2.5e-09"),
            (f64::INFINITY, "inf"),
            (f64::NAN, "nan"),
        ] {
            assert_eq!(format_g(v), expect, "%g of {v}");
        }
    }

    #[test]
    fn test_create_and_find() {
        let mut pl = ParamList::new(1);
        let i0 = pl.create_param("TEMP", ParamType::Float64).unwrap();
        let i1 = pl.create_param("COUNT", ParamType::Int32).unwrap();
        assert_eq!(i0, 0);
        assert_eq!(i1, 1);
        assert_eq!(pl.find_param("TEMP"), Some(0));
        assert_eq!(pl.find_param("COUNT"), Some(1));
        assert_eq!(pl.find_param("NOPE"), None);
        // Duplicate create returns same index
        assert_eq!(pl.create_param("TEMP", ParamType::Float64).unwrap(), 0);
    }

    #[test]
    fn test_create_param_strict_duplicate_returns_already_exists() {
        // C parity: paramList::createParam (asynPortDriver.cpp:130) —
        // a second createParam with an already-known name returns
        // `asynParamAlreadyExists`; the asynPortDriver wrapper
        // (asynPortDriver.cpp:991-1012) translates that to
        // `asynError` with a TRACE_ERROR log. The lax `create_param`
        // intentionally absorbs the duplicate for `ad-core-rs` /
        // `ad-plugins-rs` idempotent build chains; the strict
        // variant is what C-parity-sensitive callers reach for.
        let mut pl = ParamList::new(1);
        let idx = pl.create_param_strict("VAL", ParamType::Int32).unwrap();
        assert_eq!(idx, 0);
        match pl.create_param_strict("VAL", ParamType::Int32) {
            Err(AsynError::ParamAlreadyExists(name)) => assert_eq!(name, "VAL"),
            other => panic!("expected ParamAlreadyExists, got {other:?}"),
        }
        // Strict and lax variants share the registry: a name created
        // via the lax path must still be visible to strict and
        // produce ParamAlreadyExists.
        match pl.create_param_strict("VAL", ParamType::Int32) {
            Err(AsynError::ParamAlreadyExists(_)) => {}
            other => panic!("strict must observe lax-created names, got {other:?}"),
        }
        // ...and vice versa.
        assert_eq!(pl.create_param("VAL", ParamType::Int32).unwrap(), 0);
    }

    #[test]
    fn test_create_param_strict_distinct_names_succeed() {
        let mut pl = ParamList::new(1);
        let a = pl.create_param_strict("A", ParamType::Int32).unwrap();
        let b = pl.create_param_strict("B", ParamType::Float64).unwrap();
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(pl.find_param("A"), Some(0));
        assert_eq!(pl.find_param("B"), Some(1));
    }

    #[test]
    fn test_uint32_set_get_clear_interrupt_masks() {
        // C parity: paramList::setUInt32Interrupt /
        // paramList::clearUInt32Interrupt / paramList::getUInt32Interrupt
        // at asynPortDriver.cpp:480-535. ZeroToOne writes the rising
        // mask only; OneToZero writes the falling mask only; Both
        // overwrites them together. clear strips bits from BOTH
        // masks. get(Both) returns rising | falling.
        let mut pl = ParamList::new(1);
        let idx = pl
            .create_param_strict("BITS", ParamType::UInt32Digital)
            .unwrap();

        pl.set_uint32_interrupt(idx, 0, 0xF0, InterruptReason::ZeroToOne)
            .unwrap();
        assert_eq!(
            pl.get_uint32_interrupt(idx, 0, InterruptReason::ZeroToOne)
                .unwrap(),
            0xF0
        );
        assert_eq!(
            pl.get_uint32_interrupt(idx, 0, InterruptReason::OneToZero)
                .unwrap(),
            0x00
        );

        pl.set_uint32_interrupt(idx, 0, 0x0F, InterruptReason::OneToZero)
            .unwrap();
        assert_eq!(
            pl.get_uint32_interrupt(idx, 0, InterruptReason::Both)
                .unwrap(),
            0xFF
        );

        // clear strips from both masks.
        pl.clear_uint32_interrupt(idx, 0, 0x10).unwrap();
        assert_eq!(
            pl.get_uint32_interrupt(idx, 0, InterruptReason::ZeroToOne)
                .unwrap(),
            0xE0
        );
        // No falling bit at 0x10 to begin with; clear is a no-op there.
        assert_eq!(
            pl.get_uint32_interrupt(idx, 0, InterruptReason::OneToZero)
                .unwrap(),
            0x0F
        );

        // Both overwrites symmetric.
        pl.set_uint32_interrupt(idx, 0, 0xAA, InterruptReason::Both)
            .unwrap();
        assert_eq!(
            pl.get_uint32_interrupt(idx, 0, InterruptReason::ZeroToOne)
                .unwrap(),
            0xAA
        );
        assert_eq!(
            pl.get_uint32_interrupt(idx, 0, InterruptReason::OneToZero)
                .unwrap(),
            0xAA
        );
        assert_eq!(
            pl.get_uint32_interrupt(idx, 0, InterruptReason::Both)
                .unwrap(),
            0xAA
        );
    }

    #[test]
    fn test_uint32_interrupt_type_mismatch_rejects_non_uint32() {
        // C parity: setUInt32Interrupt/getUInt32Interrupt/clearUInt32Interrupt
        // return `asynParamWrongType` when called on a non-UInt32Digital
        // param (asynPortDriver.cpp:483/507/522).
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("VAL", ParamType::Int32).unwrap();
        match pl.set_uint32_interrupt(idx, 0, 0xFF, InterruptReason::Both) {
            Err(AsynError::TypeMismatch { expected, .. }) => {
                assert_eq!(expected, "UInt32Digital")
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
        match pl.clear_uint32_interrupt(idx, 0, 0xFF) {
            Err(AsynError::TypeMismatch { .. }) => {}
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
        match pl.get_uint32_interrupt(idx, 0, InterruptReason::Both) {
            Err(AsynError::TypeMismatch { .. }) => {}
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn test_get_set_int32() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("VAL", ParamType::Int32).unwrap();
        assert_eq!(pl.get_int32(idx, 0).unwrap(), 0);
        pl.set_int32(idx, 0, 42).unwrap();
        assert_eq!(pl.get_int32(idx, 0).unwrap(), 42);
    }

    #[test]
    fn test_get_set_float64() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("TEMP", ParamType::Float64).unwrap();
        pl.set_float64(idx, 0, 3.14).unwrap();
        assert!((pl.get_float64(idx, 0).unwrap() - 3.14).abs() < 1e-10);
    }

    /// C stores an octet parameter as `char *sval` (`paramVal.h`), so the byte
    /// a device sent is the byte `getStringParam` returns. Nothing in that path
    /// is text, and an embedded NUL or a lone `0x80` has to survive it.
    #[test]
    fn an_octet_parameter_round_trips_a_non_utf8_byte() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("MSG", ParamType::Octet).unwrap();
        pl.set_string(idx, 0, vec![0x80, 0x00, 0xfe, 0xff]).unwrap();
        assert_eq!(pl.get_string(idx, 0).unwrap(), &[0x80, 0x00, 0xfe, 0xff]);
    }

    #[test]
    fn test_get_set_string() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("MSG", ParamType::Octet).unwrap();
        pl.set_string(idx, 0, "hello").unwrap();
        assert_eq!(pl.get_string(idx, 0).unwrap(), b"hello");
    }

    #[test]
    fn test_get_set_uint32_mask() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("BITS", ParamType::UInt32Digital).unwrap();
        pl.set_uint32(idx, 0, 0xFF, 0x0F, 0).unwrap();
        assert_eq!(pl.get_uint32(idx, 0).unwrap(), 0x0F);
        pl.set_uint32(idx, 0, 0xFF, 0xF0, 0).unwrap();
        assert_eq!(pl.get_uint32(idx, 0).unwrap(), 0xFF);
    }

    #[test]
    fn test_multi_addr_isolation() {
        let mut pl = ParamList::new(3);
        let idx = pl.create_param("VAL", ParamType::Int32).unwrap();
        pl.set_int32(idx, 0, 10).unwrap();
        pl.set_int32(idx, 1, 20).unwrap();
        pl.set_int32(idx, 2, 30).unwrap();
        assert_eq!(pl.get_int32(idx, 0).unwrap(), 10);
        assert_eq!(pl.get_int32(idx, 1).unwrap(), 20);
        assert_eq!(pl.get_int32(idx, 2).unwrap(), 30);
    }

    /// One case per boundary of C's `getAddress` rule
    /// (asynPortDriver.cpp:1905-1913), on a port with more than one list so
    /// the sentinel and the range check are separable.
    #[test]
    fn test_addr_out_of_range() {
        let pl = ParamList::new(2);
        // -1 is the asynManager "no device" sentinel and becomes list 0; C
        // rewrites it before the range check, so it is not out of range.
        assert_eq!(pl.validate_addr(-1).unwrap(), 0);
        // Only -1 is rewritten. Any other negative fails the same check.
        assert!(pl.validate_addr(-2).is_err());
        assert_eq!(pl.validate_addr(0).unwrap(), 0);
        assert_eq!(pl.validate_addr(1).unwrap(), 1);
        assert!(pl.validate_addr(2).is_err());
    }

    /// The same rule on a one-list port: C gives it `maxAddr == 1` rather than
    /// a second rule, so 0 and the sentinel reach the list and everything else
    /// is refused — where the flag-selected rule silently served list 0.
    #[test]
    fn test_addr_normalize_single_device() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("V", ParamType::Int32).unwrap();
        pl.set_int32(idx, 0, 99).unwrap();
        assert_eq!(pl.get_int32(idx, -1).unwrap(), 99);
        assert_eq!(pl.get_int32(idx, 0).unwrap(), 99);
        assert!(matches!(
            pl.get_int32(idx, 5),
            Err(AsynError::AddressOutOfRange(5))
        ));
    }

    #[test]
    fn test_index_out_of_range() {
        let pl = ParamList::new(1);
        assert!(pl.get_int32(999, 0).is_err());
    }

    #[test]
    fn test_type_mismatch() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("VAL", ParamType::Int32).unwrap();
        assert!(pl.get_float64(idx, 0).is_err());
        assert!(pl.set_float64(idx, 0, 1.0).is_err());
    }

    #[test]
    fn test_change_tracking() {
        let mut pl = ParamList::new(1);
        let i0 = pl.create_param("A", ParamType::Int32).unwrap();
        let i1 = pl.create_param("B", ParamType::Float64).unwrap();

        pl.set_int32(i0, 0, 1).unwrap();
        pl.set_float64(i1, 0, 2.0).unwrap();

        let changed = pl.take_changed(0).unwrap();
        assert_eq!(changed.len(), 2);
        assert_eq!(changed[0], 0);
        assert_eq!(changed[1], 1);

        // Second call returns empty
        let changed2 = pl.take_changed(0).unwrap();
        assert!(changed2.is_empty());
    }

    #[test]
    fn test_same_value_no_change() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("V", ParamType::Int32).unwrap();
        pl.set_int32(idx, 0, 42).unwrap();
        let _ = pl.take_changed(0).unwrap(); // clear

        // Set same value
        pl.set_int32(idx, 0, 42).unwrap();
        let changed = pl.take_changed(0).unwrap();
        assert!(changed.is_empty());
    }

    #[test]
    fn test_array_params() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("WF", ParamType::Float64Array).unwrap();
        pl.set_float64_array(idx, 0, vec![1.0, 2.0, 3.0]).unwrap();
        let arr = pl.get_float64_array(idx, 0).unwrap();
        assert_eq!(&*arr, &[1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_param_status() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("V", ParamType::Int32).unwrap();
        pl.set_param_status(idx, 0, AsynStatus::Timeout, 1, 2)
            .unwrap();
        let (st, as_, sev) = pl.get_param_status(idx, 0).unwrap();
        assert_eq!(st, AsynStatus::Timeout);
        assert_eq!(as_, 1);
        assert_eq!(sev, 2);
    }

    #[test]
    fn test_param_name_and_type() {
        let mut pl = ParamList::new(1);
        pl.create_param("TEMP", ParamType::Float64).unwrap();
        assert_eq!(pl.param_name(0), Some("TEMP"));
        assert_eq!(pl.param_type(0), Some(ParamType::Float64));
        assert_eq!(pl.param_name(99), None);
    }

    #[test]
    fn test_timestamp_none_by_default() {
        let mut pl = ParamList::new(1);
        pl.create_param("V", ParamType::Int32).unwrap();
        assert_eq!(pl.get_timestamp(0, 0).unwrap(), None);
    }

    #[test]
    fn test_timestamp_set_get() {
        let mut pl = ParamList::new(1);
        pl.create_param("V", ParamType::Int32).unwrap();
        let ts = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(12345);
        pl.set_timestamp(0, 0, ts).unwrap();
        assert_eq!(pl.get_timestamp(0, 0).unwrap(), Some(ts));
    }

    #[test]
    fn test_take_changed_returns_indices() {
        let mut pl = ParamList::new(1);
        pl.create_param("A", ParamType::Int32).unwrap();
        pl.create_param("B", ParamType::Float64).unwrap();
        pl.create_param("C", ParamType::Octet).unwrap();

        pl.set_int32(0, 0, 1).unwrap();
        pl.set_string(2, 0, "x").unwrap();

        let changed = pl.take_changed(0).unwrap();
        assert_eq!(changed, vec![0, 2]);
    }

    // --- Enum tests ---

    #[test]
    fn test_enum_default_sentinel() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("MODE", ParamType::Enum).unwrap();
        let (index, choices) = pl.get_enum(idx, 0).unwrap();
        assert_eq!(index, 0);
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].string, "");
        assert_eq!(choices[0].value, 0);
        assert_eq!(choices[0].severity, 0);
    }

    #[test]
    fn test_enum_set_get_index() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("MODE", ParamType::Enum).unwrap();
        let choices: Arc<[EnumEntry]> = Arc::from(vec![
            EnumEntry {
                string: "Off".into(),
                value: 0,
                severity: 0,
            },
            EnumEntry {
                string: "On".into(),
                value: 1,
                severity: 0,
            },
        ]);
        pl.set_enum_choices(idx, 0, choices).unwrap();
        pl.set_enum_index(idx, 0, 1).unwrap();
        let (index, _) = pl.get_enum(idx, 0).unwrap();
        assert_eq!(index, 1);
    }

    #[test]
    fn test_enum_index_out_of_range() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("MODE", ParamType::Enum).unwrap();
        // Default has 1 choice (sentinel), so index=1 is out of range
        assert!(pl.set_enum_index(idx, 0, 1).is_err());
    }

    #[test]
    fn test_enum_type_mismatch() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("VAL", ParamType::Int32).unwrap();
        assert!(pl.get_enum(idx, 0).is_err());
    }

    #[test]
    fn test_enum_choices_update_resets_index() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("MODE", ParamType::Enum).unwrap();
        let choices: Arc<[EnumEntry]> = Arc::from(vec![
            EnumEntry {
                string: "A".into(),
                value: 0,
                severity: 0,
            },
            EnumEntry {
                string: "B".into(),
                value: 1,
                severity: 0,
            },
            EnumEntry {
                string: "C".into(),
                value: 2,
                severity: 0,
            },
        ]);
        pl.set_enum_choices(idx, 0, choices).unwrap();
        pl.set_enum_index(idx, 0, 2).unwrap();
        // Now shrink choices — index 2 is out of range, should reset to 0
        let new_choices: Arc<[EnumEntry]> = Arc::from(vec![EnumEntry {
            string: "X".into(),
            value: 0,
            severity: 0,
        }]);
        pl.set_enum_choices(idx, 0, new_choices).unwrap();
        let (index, choices) = pl.get_enum(idx, 0).unwrap();
        assert_eq!(index, 0);
        assert_eq!(choices.len(), 1);
    }

    #[test]
    fn test_enum_set_choices_marks_changed() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("MODE", ParamType::Enum).unwrap();
        let _ = pl.take_changed(0).unwrap(); // clear initial
        let choices: Arc<[EnumEntry]> = Arc::from(vec![EnumEntry {
            string: "A".into(),
            value: 0,
            severity: 0,
        }]);
        pl.set_enum_choices(idx, 0, choices).unwrap();
        let changed = pl.take_changed(0).unwrap();
        assert!(changed.contains(&idx));
    }

    // --- GenericPointer tests ---

    #[test]
    fn test_generic_pointer_default() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("PTR", ParamType::GenericPointer).unwrap();
        let val = pl.get_generic_pointer(idx, 0).unwrap();
        assert!(val.downcast_ref::<()>().is_some());
    }

    #[test]
    fn test_generic_pointer_set_get_downcast() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("PTR", ParamType::GenericPointer).unwrap();
        let data: Arc<dyn Any + Send + Sync> = Arc::new(42i32);
        pl.set_generic_pointer(idx, 0, data).unwrap();
        let val = pl.get_generic_pointer(idx, 0).unwrap();
        assert_eq!(*val.downcast_ref::<i32>().unwrap(), 42);
    }

    #[test]
    fn test_generic_pointer_downcast_wrong_type() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("PTR", ParamType::GenericPointer).unwrap();
        let data: Arc<dyn Any + Send + Sync> = Arc::new(42i32);
        pl.set_generic_pointer(idx, 0, data).unwrap();
        let val = pl.get_generic_pointer(idx, 0).unwrap();
        assert!(val.downcast_ref::<String>().is_none());
    }

    #[test]
    fn test_generic_pointer_type_mismatch() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("VAL", ParamType::Int32).unwrap();
        assert!(pl.get_generic_pointer(idx, 0).is_err());
    }

    #[test]
    fn test_generic_pointer_debug_shows_type_id() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("PTR", ParamType::GenericPointer).unwrap();
        let data: Arc<dyn Any + Send + Sync> = Arc::new(vec![1, 2, 3]);
        pl.set_generic_pointer(idx, 0, data).unwrap();
        let val = pl.get_value(idx, 0).unwrap();
        let s = format!("{val:?}");
        assert!(s.contains("GenericPointer"));
        assert!(s.contains("TypeId"));
    }

    // --- Int64 tests ---

    #[test]
    fn test_get_set_int64() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("BIG", ParamType::Int64).unwrap();
        assert_eq!(pl.get_int64(idx, 0).unwrap(), 0);
        pl.set_int64(idx, 0, i64::MAX).unwrap();
        assert_eq!(pl.get_int64(idx, 0).unwrap(), i64::MAX);
    }

    #[test]
    fn test_int64_type_mismatch() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("V", ParamType::Int32).unwrap();
        assert!(pl.get_int64(idx, 0).is_err());
        assert!(pl.set_int64(idx, 0, 1).is_err());
    }

    #[test]
    fn test_int64_same_value_no_change() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("V", ParamType::Int64).unwrap();
        pl.set_int64(idx, 0, 42).unwrap();
        let _ = pl.take_changed(0).unwrap();
        pl.set_int64(idx, 0, 42).unwrap();
        let changed = pl.take_changed(0).unwrap();
        assert!(changed.is_empty());
    }

    #[test]
    fn test_int64_change_tracking() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("V", ParamType::Int64).unwrap();
        pl.set_int64(idx, 0, 100).unwrap();
        let changed = pl.take_changed(0).unwrap();
        assert_eq!(changed, vec![idx]);
    }

    // --- AsynParamSet (C++ asynParamSet equivalent) ---

    #[test]
    fn asyn_param_set_add_returns_slot_index_in_order() {
        let mut set = AsynParamSet::new();
        // C++ `add()` returns void; we expose the slot index so a
        // Rust caller can recover indices via the Vec returned by
        // create_all(). Order matters: registration order == slot
        // order == result-Vec order.
        assert_eq!(set.add("Temperature", ParamType::Float64), 0);
        assert_eq!(set.add("Status", ParamType::Int32), 1);
        assert_eq!(set.add("Tag", ParamType::Octet), 2);
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn asyn_param_set_create_all_assigns_indices_in_order() {
        let mut set = AsynParamSet::new();
        let temp_slot = set.add("Temperature", ParamType::Float64);
        let status_slot = set.add("Status", ParamType::Int32);
        let tag_slot = set.add("Tag", ParamType::Octet);

        let mut pl = ParamList::new(1);
        let indices = set.create_all(&mut pl).unwrap();
        assert_eq!(indices.len(), 3);
        assert_eq!(indices[temp_slot], 0);
        assert_eq!(indices[status_slot], 1);
        assert_eq!(indices[tag_slot], 2);
        // ParamList agrees on the names.
        assert_eq!(pl.find_param("Temperature"), Some(0));
        assert_eq!(pl.find_param("Status"), Some(1));
        assert_eq!(pl.find_param("Tag"), Some(2));
    }

    #[test]
    fn asyn_param_set_iter_preserves_registration_order() {
        let mut set = AsynParamSet::new();
        set.add("A", ParamType::Int32);
        set.add("B", ParamType::Float64);
        set.add("C", ParamType::Octet);
        let names: Vec<&str> = set.iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["A", "B", "C"]);
    }

    #[test]
    fn asyn_param_set_empty_create_all_is_noop() {
        let set = AsynParamSet::new();
        let mut pl = ParamList::new(1);
        let idx = set.create_all(&mut pl).unwrap();
        assert!(idx.is_empty());
        assert!(pl.is_empty());
    }

    #[test]
    fn asyn_param_set_duplicate_name_returns_existing_index() {
        // ParamList::create_param returns the existing index on dup
        // (silent dedup); AsynParamSet inherits that — two add()
        // calls with the same name resolve to the same final index.
        // Matches C asyn where createParam dedupes via name_to_index.
        let mut set = AsynParamSet::new();
        set.add("X", ParamType::Int32);
        set.add("X", ParamType::Int32);
        let mut pl = ParamList::new(1);
        let idx = set.create_all(&mut pl).unwrap();
        assert_eq!(idx, vec![0, 0]);
    }

    // ------------------------------------------------------------------
    // Strict-getter / C-parity tests for asynParamUndefined.
    // The lax `get_*` variants stay at the type default; the `_strict`
    // variants surface the C status. The setters now flip `defined` on a
    // first set even when the value equals the type default
    // (C `paramVal::setInteger/setDouble/setString/setUInt32`).
    // ------------------------------------------------------------------

    #[test]
    fn get_int32_strict_undefined_returns_param_undefined() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("U", ParamType::Int32).unwrap();
        // Lax getter still works (returns the type default).
        assert_eq!(pl.get_int32(idx, 0).unwrap(), 0);
        // Strict getter surfaces asynParamUndefined.
        let err = pl.get_int32_strict(idx, 0).unwrap_err();
        assert!(matches!(err, AsynError::ParamUndefined(i) if i == idx));
        pl.set_int32(idx, 0, 7).unwrap();
        assert_eq!(pl.get_int32_strict(idx, 0).unwrap(), 7);
    }

    #[test]
    fn get_float64_strict_undefined_returns_param_undefined() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("U", ParamType::Float64).unwrap();
        assert_eq!(pl.get_float64(idx, 0).unwrap(), 0.0);
        assert!(matches!(
            pl.get_float64_strict(idx, 0).unwrap_err(),
            AsynError::ParamUndefined(i) if i == idx
        ));
    }

    #[test]
    fn get_int64_strict_undefined_returns_param_undefined() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("U", ParamType::Int64).unwrap();
        assert!(matches!(
            pl.get_int64_strict(idx, 0).unwrap_err(),
            AsynError::ParamUndefined(i) if i == idx
        ));
    }

    #[test]
    fn get_uint32_strict_undefined_returns_param_undefined() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("U", ParamType::UInt32Digital).unwrap();
        assert!(matches!(
            pl.get_uint32_strict(idx, 0).unwrap_err(),
            AsynError::ParamUndefined(i) if i == idx
        ));
    }

    #[test]
    fn get_string_strict_undefined_returns_param_undefined() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("U", ParamType::Octet).unwrap();
        assert!(matches!(
            pl.get_string_strict(idx, 0).unwrap_err(),
            AsynError::ParamUndefined(i) if i == idx
        ));
    }

    #[test]
    fn get_strict_checks_type_before_undefined_c_parity() {
        // C parity: paramVal::getInteger throws ParamValWrongType before
        // ParamValNotDefined (paramVal.cpp:147-154). Reading a Float64
        // param via get_int32_strict on a never-set entry must surface
        // TypeMismatch, not ParamUndefined.
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("F", ParamType::Float64).unwrap();
        assert!(matches!(
            pl.get_int32_strict(idx, 0).unwrap_err(),
            AsynError::TypeMismatch {
                expected: "Int32",
                ..
            }
        ));
    }

    #[test]
    fn set_int32_first_write_with_default_value_flips_defined() {
        // C parity: paramVal::setInteger checks `!isDefined() || value != old`
        // so writing `0` to a fresh Int32 still flips defined+value_changed.
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("V", ParamType::Int32).unwrap();
        assert!(!pl.is_param_defined(idx, 0).unwrap());
        pl.set_int32(idx, 0, 0).unwrap();
        assert!(pl.is_param_defined(idx, 0).unwrap());
        assert!(pl.take_changed_single(idx, 0).unwrap());
        assert_eq!(pl.get_int32_strict(idx, 0).unwrap(), 0);
    }

    #[test]
    fn set_float64_first_write_with_default_value_flips_defined() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("V", ParamType::Float64).unwrap();
        pl.set_float64(idx, 0, 0.0).unwrap();
        assert!(pl.is_param_defined(idx, 0).unwrap());
        assert_eq!(pl.get_float64_strict(idx, 0).unwrap(), 0.0);
    }

    #[test]
    fn set_uint32_first_write_zero_mask_zero_flips_defined() {
        // C parity: paramVal::setUInt32 (paramVal.cpp:200-208) flips
        // defined+value_changed on the first set even when the resulting
        // value is 0 — driver code commonly does `setUIntDigitalParam(idx, 0, mask)`
        // to publish an initial zero state and expects the callback to fire.
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("V", ParamType::UInt32Digital).unwrap();
        pl.set_uint32(idx, 0, 0, 0xFFFF, 0).unwrap();
        assert!(pl.is_param_defined(idx, 0).unwrap());
        assert!(pl.take_changed_single(idx, 0).unwrap());
        assert_eq!(pl.get_uint32_strict(idx, 0).unwrap(), 0);
    }

    #[test]
    fn set_string_first_write_empty_flips_defined() {
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("V", ParamType::Octet).unwrap();
        pl.set_string(idx, 0, Vec::new()).unwrap();
        assert!(pl.is_param_defined(idx, 0).unwrap());
        assert_eq!(pl.get_string_strict(idx, 0).unwrap(), b"");
    }

    #[test]
    fn set_enum_index_first_write_to_default_index_flips_defined() {
        // C parity: enum index is stored through paramVal::setInteger,
        // which gates on `!isDefined() || data != value` — a first set
        // to index 0 (the type default) must still flip defined and
        // value_changed so the initial selection fires its I/O Intr.
        // A freshly created Enum param carries one sentinel choice and
        // is undefined; set_enum_index(.., 0) must define it.
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("MODE", ParamType::Enum).unwrap();
        assert!(!pl.is_param_defined(idx, 0).unwrap());
        pl.set_enum_index(idx, 0, 0).unwrap();
        assert!(pl.is_param_defined(idx, 0).unwrap());
        assert!(pl.take_changed_single(idx, 0).unwrap());
    }

    #[test]
    fn set_param_status_only_change_marks_value_changed() {
        // C paramVal::setStatus (paramVal.cpp:71-78) calls setValueChanged()
        // on a status transition with no value change, so the standard
        // setParamStatus() + callParamCallbacks() alarm-push delivers an
        // I/O Intr. The param value itself is never touched here.
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("S", ParamType::Int32).unwrap();
        let _ = pl.take_changed(0).unwrap();
        pl.set_param_status(idx, 0, AsynStatus::Timeout, 0, 0)
            .unwrap();
        let changed = pl.take_changed(0).unwrap();
        assert!(
            changed.contains(&idx),
            "a status-only transition must mark the param value_changed"
        );
    }

    #[test]
    fn set_param_alarm_only_change_marks_value_changed() {
        // C setAlarmStatus / setAlarmSeverity (paramVal.cpp:84-91,97-104)
        // also call setValueChanged() on change — status can stay Success.
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("S", ParamType::Float64).unwrap();
        let _ = pl.take_changed(0).unwrap();
        pl.set_param_status(idx, 0, AsynStatus::Success, 7, 2)
            .unwrap();
        assert!(
            pl.take_changed_single(idx, 0).unwrap(),
            "an alarm-status/severity-only transition must mark the param value_changed"
        );
    }

    #[test]
    fn set_param_status_change_forces_full_uint32_mask() {
        // C setStatus on a UInt32Digital param forces uInt32CallbackMask =
        // 0xFFFFFFFF (paramVal.cpp:76) so every subscriber bit fires.
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("U", ParamType::UInt32Digital).unwrap();
        let _ = pl.take_changed(0).unwrap();
        pl.set_param_status(idx, 0, AsynStatus::Error, 0, 0)
            .unwrap();
        assert!(pl.take_changed_single(idx, 0).unwrap());
        assert_eq!(
            pl.get_uint32_interrupt_mask(idx, 0).unwrap(),
            0xFFFF_FFFF,
            "a UInt32Digital status change must force the full callback mask"
        );
    }

    #[test]
    fn set_uint32_force_interrupt_mask_on_unchanged_value_notifies() {
        // C paramVal::setUInt32 (paramVal.cpp:220-224): a non-zero
        // `interruptMask` ORs those bits into uInt32CallbackMask AND calls
        // setValueChanged() UNCONDITIONALLY — even when the merged value is
        // identical to the stored value. This is the setUIntDigitalParam
        // interruptMask overload (asynPortDriver.cpp:1381): a driver forces
        // an I/O Intr on specific bits without changing the value.
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("U", ParamType::UInt32Digital).unwrap();
        // Define the param at 0x05 and drain the resulting change + mask.
        pl.set_uint32(idx, 0, 0x05, 0x0F, 0).unwrap();
        let _ = pl.take_changed(0).unwrap();
        let _ = pl.take_uint32_interrupt_mask(idx, 0).unwrap();
        // Re-set the SAME value (no value-bit change) but force bit 0x02.
        pl.set_uint32(idx, 0, 0x05, 0x0F, 0x02).unwrap();
        assert_eq!(
            pl.get_uint32(idx, 0).unwrap(),
            0x05,
            "a forced-interrupt-only set must not change the stored value"
        );
        assert!(
            pl.take_changed(0).unwrap().contains(&idx),
            "a forced interruptMask must mark value_changed even on an unchanged value"
        );
        assert_eq!(
            pl.take_uint32_interrupt_mask(idx, 0).unwrap(),
            0x02,
            "the forced interruptMask bits must land in the callback mask"
        );
    }

    #[test]
    fn set_uint32_accumulates_callback_mask_across_sets() {
        // C `uInt32CallbackMask |= (uival ^ newValue)` (paramVal.cpp:215):
        // two setUInt32 calls before one callParamCallbacks must accumulate
        // the union of changed bits, not keep only the last set's bits.
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("U", ParamType::UInt32Digital).unwrap();
        pl.set_uint32(idx, 0, 0x01, 0x01, 0).unwrap(); // define + change bit 0
        pl.set_uint32(idx, 0, 0x02, 0x02, 0).unwrap(); // change bit 1
        assert_eq!(
            pl.get_uint32_interrupt_mask(idx, 0).unwrap(),
            0x03,
            "callback mask must accumulate the union of changed bits since the last flush"
        );
    }

    #[test]
    fn take_uint32_interrupt_mask_reads_and_resets() {
        // C uint32Callback resets uInt32CallbackMask = 0 after firing
        // (asynPortDriver.cpp:855).
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("U", ParamType::UInt32Digital).unwrap();
        pl.set_uint32(idx, 0, 0x05, 0x0F, 0).unwrap();
        assert_eq!(pl.take_uint32_interrupt_mask(idx, 0).unwrap(), 0x05);
        assert_eq!(
            pl.get_uint32_interrupt_mask(idx, 0).unwrap(),
            0,
            "callback mask must be reset to 0 after take"
        );
    }

    #[test]
    fn set_param_status_no_change_does_not_mark_value_changed() {
        // C gates on `status_ != status` etc., so re-asserting the
        // existing status/alarm (the fresh-param default Success/0/0) is a
        // no-op and must NOT mark the param dirty.
        let mut pl = ParamList::new(1);
        let idx = pl.create_param("S", ParamType::Int32).unwrap();
        let _ = pl.take_changed(0).unwrap();
        pl.set_param_status(idx, 0, AsynStatus::Success, 0, 0)
            .unwrap();
        assert!(
            !pl.take_changed_single(idx, 0).unwrap(),
            "re-asserting the same status/alarm must not mark the param value_changed"
        );
    }
}
