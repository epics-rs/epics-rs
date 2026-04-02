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

/// Parameter data types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    Int32,
    Int64,
    Float64,
    Octet,
    UInt32Digital,
    Int8Array,
    Int16Array,
    Int32Array,
    Int64Array,
    Float32Array,
    Float64Array,
    Enum,
    GenericPointer,
}

/// Parameter value. Arrays use `Arc<[T]>` for cheap cloning during interrupt broadcast.
/// Octet uses String (typically short; not intended for large binary data).
#[derive(Clone)]
pub enum ParamValue {
    Int32(i32),
    Int64(i64),
    Float64(f64),
    Octet(String),
    UInt32Digital(u32),
    Int8Array(Arc<[i8]>),
    Int16Array(Arc<[i16]>),
    Int32Array(Arc<[i32]>),
    Int64Array(Arc<[i64]>),
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
            Self::Float64(v) => write!(f, "Float64({v:?})"),
            Self::Octet(v) => write!(f, "Octet({v:?})"),
            Self::UInt32Digital(v) => write!(f, "UInt32Digital({v:?})"),
            Self::Int8Array(v) => write!(f, "Int8Array({v:?})"),
            Self::Int16Array(v) => write!(f, "Int16Array({v:?})"),
            Self::Int32Array(v) => write!(f, "Int32Array({v:?})"),
            Self::Int64Array(v) => write!(f, "Int64Array({v:?})"),
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
            Self::Float64(_) => "Float64",
            Self::Octet(_) => "Octet",
            Self::UInt32Digital(_) => "UInt32Digital",
            Self::Int8Array(_) => "Int8Array",
            Self::Int16Array(_) => "Int16Array",
            Self::Int32Array(_) => "Int32Array",
            Self::Int64Array(_) => "Int64Array",
            Self::Float32Array(_) => "Float32Array",
            Self::Float64Array(_) => "Float64Array",
            Self::Enum { .. } => "Enum",
            Self::GenericPointer(_) => "GenericPointer",
            Self::Undefined => "Undefined",
        }
    }
}

#[derive(Debug, Clone)]
struct ParamEntry {
    name: String,
    param_type: ParamType,
    value: ParamValue,
    status: AsynStatus,
    alarm_status: u16,
    alarm_severity: u16,
    value_changed: bool,
    timestamp: Option<SystemTime>,
}

impl ParamEntry {
    fn new(name: String, param_type: ParamType) -> Self {
        let value = match param_type {
            ParamType::Int32 => ParamValue::Int32(0),
            ParamType::Int64 => ParamValue::Int64(0),
            ParamType::Float64 => ParamValue::Float64(0.0),
            ParamType::Octet => ParamValue::Octet(String::new()),
            ParamType::UInt32Digital => ParamValue::UInt32Digital(0),
            ParamType::Int8Array => ParamValue::Int8Array(Arc::from([] as [i8; 0])),
            ParamType::Int16Array => ParamValue::Int16Array(Arc::from([] as [i16; 0])),
            ParamType::Int32Array => ParamValue::Int32Array(Arc::from([] as [i32; 0])),
            ParamType::Int64Array => ParamValue::Int64Array(Arc::from([] as [i64; 0])),
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
            status: AsynStatus::Success,
            alarm_status: 0,
            alarm_severity: 0,
            value_changed: false,
            timestamp: None,
        }
    }
}

/// Parameter library managing named parameters indexed by integer.
/// Supports multiple addresses (addr 0..max_addr).
pub struct ParamList {
    max_addr: usize,
    multi_device: bool,
    /// params[addr][index] = ParamEntry
    params: Vec<Vec<ParamEntry>>,
    name_to_index: HashMap<String, usize>,
}

impl ParamList {
    pub fn new(max_addr: usize, multi_device: bool) -> Self {
        let max_addr = max_addr.max(1);
        Self {
            max_addr,
            multi_device,
            params: (0..max_addr).map(|_| Vec::new()).collect(),
            name_to_index: HashMap::new(),
        }
    }

    /// Normalize and validate address.
    /// - multi_device=false: any addr normalizes to 0
    /// - multi_device=true: addr must be in [0, max_addr)
    fn validate_addr(&self, addr: i32) -> AsynResult<usize> {
        if !self.multi_device {
            return Ok(0);
        }
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
    pub fn create_param(&mut self, name: &str, param_type: ParamType) -> AsynResult<usize> {
        if let Some(&idx) = self.name_to_index.get(name) {
            return Ok(idx);
        }
        let index = if self.params[0].is_empty() {
            0
        } else {
            self.params[0].len()
        };
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

    pub fn get_int32(&self, index: usize, addr: i32) -> AsynResult<i32> {
        match &self.get_entry(index, addr)?.value {
            ParamValue::Int32(v) => Ok(*v),
            // C EPICS asyn: asynInt32 interface reads enum index transparently
            ParamValue::Enum { index: idx, .. } => Ok(*idx as i32),
            other => Err(AsynError::TypeMismatch {
                expected: "Int32",
                actual: other.type_name(),
            }),
        }
    }

    pub fn set_int32(&mut self, index: usize, addr: i32, value: i32) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        match entry.value {
            ParamValue::Int32(ref old) => {
                if *old != value {
                    entry.value = ParamValue::Int32(value);
                    entry.value_changed = true;
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
                if *index != new_idx {
                    *index = new_idx;
                    entry.value_changed = true;
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

    pub fn get_float64(&self, index: usize, addr: i32) -> AsynResult<f64> {
        match &self.get_entry(index, addr)?.value {
            ParamValue::Float64(v) => Ok(*v),
            other => Err(AsynError::TypeMismatch {
                expected: "Float64",
                actual: other.type_name(),
            }),
        }
    }

    pub fn set_float64(&mut self, index: usize, addr: i32, value: f64) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        if let ParamValue::Float64(ref old) = entry.value {
            if *old != value {
                entry.value = ParamValue::Float64(value);
                entry.value_changed = true;
            }
        } else {
            return Err(AsynError::TypeMismatch {
                expected: "Float64",
                actual: entry.value.type_name(),
            });
        }
        Ok(())
    }

    pub fn get_int64(&self, index: usize, addr: i32) -> AsynResult<i64> {
        match &self.get_entry(index, addr)?.value {
            ParamValue::Int64(v) => Ok(*v),
            other => Err(AsynError::TypeMismatch {
                expected: "Int64",
                actual: other.type_name(),
            }),
        }
    }

    pub fn set_int64(&mut self, index: usize, addr: i32, value: i64) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        if let ParamValue::Int64(ref old) = entry.value {
            if *old != value {
                entry.value = ParamValue::Int64(value);
                entry.value_changed = true;
            }
        } else {
            return Err(AsynError::TypeMismatch {
                expected: "Int64",
                actual: entry.value.type_name(),
            });
        }
        Ok(())
    }

    pub fn get_string(&self, index: usize, addr: i32) -> AsynResult<&str> {
        match &self.get_entry(index, addr)?.value {
            ParamValue::Octet(s) => Ok(s),
            other => Err(AsynError::TypeMismatch {
                expected: "Octet",
                actual: other.type_name(),
            }),
        }
    }

    pub fn set_string(&mut self, index: usize, addr: i32, value: String) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        if let ParamValue::Octet(ref old) = entry.value {
            if *old != value {
                entry.value = ParamValue::Octet(value);
                entry.value_changed = true;
            }
        } else {
            return Err(AsynError::TypeMismatch {
                expected: "Octet",
                actual: entry.value.type_name(),
            });
        }
        Ok(())
    }

    pub fn get_uint32(&self, index: usize, addr: i32) -> AsynResult<u32> {
        match &self.get_entry(index, addr)?.value {
            ParamValue::UInt32Digital(v) => Ok(*v),
            other => Err(AsynError::TypeMismatch {
                expected: "UInt32Digital",
                actual: other.type_name(),
            }),
        }
    }

    pub fn set_uint32(&mut self, index: usize, addr: i32, value: u32, mask: u32) -> AsynResult<()> {
        let entry = self.get_entry_mut(index, addr)?;
        if let ParamValue::UInt32Digital(ref old) = entry.value {
            let new_val = (*old & !mask) | (value & mask);
            if *old != new_val {
                entry.value = ParamValue::UInt32Digital(new_val);
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
        match &self.get_entry(index, addr)?.value {
            ParamValue::Enum {
                index: idx,
                choices,
            } => Ok((*idx, choices.clone())),
            other => Err(AsynError::TypeMismatch {
                expected: "Enum",
                actual: other.type_name(),
            }),
        }
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
            if *idx != value {
                *idx = value;
                entry.value_changed = true;
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
            entry.value_changed = true; // Any is not comparable
            Ok(())
        } else {
            Err(AsynError::TypeMismatch {
                expected: "GenericPointer",
                actual: entry.value.type_name(),
            })
        }
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
        entry.status = status;
        entry.alarm_status = alarm_status;
        entry.alarm_severity = alarm_severity;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_find() {
        let mut pl = ParamList::new(1, false);
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
    fn test_get_set_int32() {
        let mut pl = ParamList::new(1, false);
        let idx = pl.create_param("VAL", ParamType::Int32).unwrap();
        assert_eq!(pl.get_int32(idx, 0).unwrap(), 0);
        pl.set_int32(idx, 0, 42).unwrap();
        assert_eq!(pl.get_int32(idx, 0).unwrap(), 42);
    }

    #[test]
    fn test_get_set_float64() {
        let mut pl = ParamList::new(1, false);
        let idx = pl.create_param("TEMP", ParamType::Float64).unwrap();
        pl.set_float64(idx, 0, 3.14).unwrap();
        assert!((pl.get_float64(idx, 0).unwrap() - 3.14).abs() < 1e-10);
    }

    #[test]
    fn test_get_set_string() {
        let mut pl = ParamList::new(1, false);
        let idx = pl.create_param("MSG", ParamType::Octet).unwrap();
        pl.set_string(idx, 0, "hello".into()).unwrap();
        assert_eq!(pl.get_string(idx, 0).unwrap(), "hello");
    }

    #[test]
    fn test_get_set_uint32_mask() {
        let mut pl = ParamList::new(1, false);
        let idx = pl.create_param("BITS", ParamType::UInt32Digital).unwrap();
        pl.set_uint32(idx, 0, 0xFF, 0x0F).unwrap();
        assert_eq!(pl.get_uint32(idx, 0).unwrap(), 0x0F);
        pl.set_uint32(idx, 0, 0xFF, 0xF0).unwrap();
        assert_eq!(pl.get_uint32(idx, 0).unwrap(), 0xFF);
    }

    #[test]
    fn test_multi_addr_isolation() {
        let mut pl = ParamList::new(3, true);
        let idx = pl.create_param("VAL", ParamType::Int32).unwrap();
        pl.set_int32(idx, 0, 10).unwrap();
        pl.set_int32(idx, 1, 20).unwrap();
        pl.set_int32(idx, 2, 30).unwrap();
        assert_eq!(pl.get_int32(idx, 0).unwrap(), 10);
        assert_eq!(pl.get_int32(idx, 1).unwrap(), 20);
        assert_eq!(pl.get_int32(idx, 2).unwrap(), 30);
    }

    #[test]
    fn test_addr_out_of_range() {
        let pl = ParamList::new(2, true);
        assert!(pl.validate_addr(-1).is_err());
        assert!(pl.validate_addr(2).is_err());
        assert!(pl.validate_addr(0).is_ok());
        assert!(pl.validate_addr(1).is_ok());
    }

    #[test]
    fn test_addr_normalize_single_device() {
        let mut pl = ParamList::new(1, false);
        let idx = pl.create_param("V", ParamType::Int32).unwrap();
        pl.set_int32(idx, 0, 99).unwrap();
        // Any addr normalizes to 0 for single-device
        assert_eq!(pl.get_int32(idx, 5).unwrap(), 99);
        assert_eq!(pl.get_int32(idx, -1).unwrap(), 99);
    }

    #[test]
    fn test_index_out_of_range() {
        let pl = ParamList::new(1, false);
        assert!(pl.get_int32(999, 0).is_err());
    }

    #[test]
    fn test_type_mismatch() {
        let mut pl = ParamList::new(1, false);
        let idx = pl.create_param("VAL", ParamType::Int32).unwrap();
        assert!(pl.get_float64(idx, 0).is_err());
        assert!(pl.set_float64(idx, 0, 1.0).is_err());
    }

    #[test]
    fn test_change_tracking() {
        let mut pl = ParamList::new(1, false);
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
        let mut pl = ParamList::new(1, false);
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
        let mut pl = ParamList::new(1, false);
        let idx = pl.create_param("WF", ParamType::Float64Array).unwrap();
        pl.set_float64_array(idx, 0, vec![1.0, 2.0, 3.0]).unwrap();
        let arr = pl.get_float64_array(idx, 0).unwrap();
        assert_eq!(&*arr, &[1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_param_status() {
        let mut pl = ParamList::new(1, false);
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
        let mut pl = ParamList::new(1, false);
        pl.create_param("TEMP", ParamType::Float64).unwrap();
        assert_eq!(pl.param_name(0), Some("TEMP"));
        assert_eq!(pl.param_type(0), Some(ParamType::Float64));
        assert_eq!(pl.param_name(99), None);
    }

    #[test]
    fn test_timestamp_none_by_default() {
        let mut pl = ParamList::new(1, false);
        pl.create_param("V", ParamType::Int32).unwrap();
        assert_eq!(pl.get_timestamp(0, 0).unwrap(), None);
    }

    #[test]
    fn test_timestamp_set_get() {
        let mut pl = ParamList::new(1, false);
        pl.create_param("V", ParamType::Int32).unwrap();
        let ts = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(12345);
        pl.set_timestamp(0, 0, ts).unwrap();
        assert_eq!(pl.get_timestamp(0, 0).unwrap(), Some(ts));
    }

    #[test]
    fn test_take_changed_returns_indices() {
        let mut pl = ParamList::new(1, false);
        pl.create_param("A", ParamType::Int32).unwrap();
        pl.create_param("B", ParamType::Float64).unwrap();
        pl.create_param("C", ParamType::Octet).unwrap();

        pl.set_int32(0, 0, 1).unwrap();
        pl.set_string(2, 0, "x".into()).unwrap();

        let changed = pl.take_changed(0).unwrap();
        assert_eq!(changed, vec![0, 2]);
    }

    // --- Enum tests ---

    #[test]
    fn test_enum_default_sentinel() {
        let mut pl = ParamList::new(1, false);
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
        let mut pl = ParamList::new(1, false);
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
        let mut pl = ParamList::new(1, false);
        let idx = pl.create_param("MODE", ParamType::Enum).unwrap();
        // Default has 1 choice (sentinel), so index=1 is out of range
        assert!(pl.set_enum_index(idx, 0, 1).is_err());
    }

    #[test]
    fn test_enum_type_mismatch() {
        let mut pl = ParamList::new(1, false);
        let idx = pl.create_param("VAL", ParamType::Int32).unwrap();
        assert!(pl.get_enum(idx, 0).is_err());
    }

    #[test]
    fn test_enum_choices_update_resets_index() {
        let mut pl = ParamList::new(1, false);
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
        let mut pl = ParamList::new(1, false);
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
        let mut pl = ParamList::new(1, false);
        let idx = pl.create_param("PTR", ParamType::GenericPointer).unwrap();
        let val = pl.get_generic_pointer(idx, 0).unwrap();
        assert!(val.downcast_ref::<()>().is_some());
    }

    #[test]
    fn test_generic_pointer_set_get_downcast() {
        let mut pl = ParamList::new(1, false);
        let idx = pl.create_param("PTR", ParamType::GenericPointer).unwrap();
        let data: Arc<dyn Any + Send + Sync> = Arc::new(42i32);
        pl.set_generic_pointer(idx, 0, data).unwrap();
        let val = pl.get_generic_pointer(idx, 0).unwrap();
        assert_eq!(*val.downcast_ref::<i32>().unwrap(), 42);
    }

    #[test]
    fn test_generic_pointer_downcast_wrong_type() {
        let mut pl = ParamList::new(1, false);
        let idx = pl.create_param("PTR", ParamType::GenericPointer).unwrap();
        let data: Arc<dyn Any + Send + Sync> = Arc::new(42i32);
        pl.set_generic_pointer(idx, 0, data).unwrap();
        let val = pl.get_generic_pointer(idx, 0).unwrap();
        assert!(val.downcast_ref::<String>().is_none());
    }

    #[test]
    fn test_generic_pointer_type_mismatch() {
        let mut pl = ParamList::new(1, false);
        let idx = pl.create_param("VAL", ParamType::Int32).unwrap();
        assert!(pl.get_generic_pointer(idx, 0).is_err());
    }

    #[test]
    fn test_generic_pointer_debug_shows_type_id() {
        let mut pl = ParamList::new(1, false);
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
        let mut pl = ParamList::new(1, false);
        let idx = pl.create_param("BIG", ParamType::Int64).unwrap();
        assert_eq!(pl.get_int64(idx, 0).unwrap(), 0);
        pl.set_int64(idx, 0, i64::MAX).unwrap();
        assert_eq!(pl.get_int64(idx, 0).unwrap(), i64::MAX);
    }

    #[test]
    fn test_int64_type_mismatch() {
        let mut pl = ParamList::new(1, false);
        let idx = pl.create_param("V", ParamType::Int32).unwrap();
        assert!(pl.get_int64(idx, 0).is_err());
        assert!(pl.set_int64(idx, 0, 1).is_err());
    }

    #[test]
    fn test_int64_same_value_no_change() {
        let mut pl = ParamList::new(1, false);
        let idx = pl.create_param("V", ParamType::Int64).unwrap();
        pl.set_int64(idx, 0, 42).unwrap();
        let _ = pl.take_changed(0).unwrap();
        pl.set_int64(idx, 0, 42).unwrap();
        let changed = pl.take_changed(0).unwrap();
        assert!(changed.is_empty());
    }

    #[test]
    fn test_int64_change_tracking() {
        let mut pl = ParamList::new(1, false);
        let idx = pl.create_param("V", ParamType::Int64).unwrap();
        pl.set_int64(idx, 0, 100).unwrap();
        let changed = pl.take_changed(0).unwrap();
        assert_eq!(changed, vec![idx]);
    }
}
