use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::param;

/// Serializable enum entry (mirrors `param::EnumEntry`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumEntry {
    pub string: String,
    pub value: i32,
    pub severity: u16,
}

impl From<&param::EnumEntry> for EnumEntry {
    fn from(e: &param::EnumEntry) -> Self {
        Self {
            string: e.string.clone(),
            value: e.value,
            severity: e.severity,
        }
    }
}

impl From<&EnumEntry> for param::EnumEntry {
    fn from(e: &EnumEntry) -> Self {
        Self {
            string: e.string.clone(),
            value: e.value,
            severity: e.severity,
        }
    }
}

/// Serializable parameter value. No GenericPointer, Vec instead of Arc.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ParamValue {
    Int32(i32),
    Int64(i64),
    Float64(f64),
    Octet(String),
    UInt32Digital(u32),
    Int8Array(Vec<i8>),
    Int16Array(Vec<i16>),
    Int32Array(Vec<i32>),
    Int64Array(Vec<i64>),
    Float32Array(Vec<f32>),
    Float64Array(Vec<f64>),
    Enum {
        index: usize,
        choices: Vec<EnumEntry>,
    },
    Undefined,
}

impl From<&param::ParamValue> for ParamValue {
    fn from(v: &param::ParamValue) -> Self {
        match v {
            param::ParamValue::Int32(n) => Self::Int32(*n),
            param::ParamValue::Int64(n) => Self::Int64(*n),
            param::ParamValue::Float64(n) => Self::Float64(*n),
            param::ParamValue::Octet(s) => Self::Octet(s.clone()),
            param::ParamValue::UInt32Digital(n) => Self::UInt32Digital(*n),
            param::ParamValue::Int8Array(a) => Self::Int8Array(a.to_vec()),
            param::ParamValue::Int16Array(a) => Self::Int16Array(a.to_vec()),
            param::ParamValue::Int32Array(a) => Self::Int32Array(a.to_vec()),
            param::ParamValue::Int64Array(a) => Self::Int64Array(a.to_vec()),
            param::ParamValue::Float32Array(a) => Self::Float32Array(a.to_vec()),
            param::ParamValue::Float64Array(a) => Self::Float64Array(a.to_vec()),
            param::ParamValue::Enum { index, choices } => Self::Enum {
                index: *index,
                choices: choices.iter().map(EnumEntry::from).collect(),
            },
            param::ParamValue::GenericPointer(_) => Self::Undefined,
            param::ParamValue::Undefined => Self::Undefined,
        }
    }
}

impl From<param::ParamValue> for ParamValue {
    fn from(v: param::ParamValue) -> Self {
        Self::from(&v)
    }
}

impl From<&ParamValue> for param::ParamValue {
    fn from(v: &ParamValue) -> Self {
        match v {
            ParamValue::Int32(n) => Self::Int32(*n),
            ParamValue::Int64(n) => Self::Int64(*n),
            ParamValue::Float64(n) => Self::Float64(*n),
            ParamValue::Octet(s) => Self::Octet(s.clone()),
            ParamValue::UInt32Digital(n) => Self::UInt32Digital(*n),
            ParamValue::Int8Array(a) => Self::Int8Array(Arc::from(a.as_slice())),
            ParamValue::Int16Array(a) => Self::Int16Array(Arc::from(a.as_slice())),
            ParamValue::Int32Array(a) => Self::Int32Array(Arc::from(a.as_slice())),
            ParamValue::Int64Array(a) => Self::Int64Array(Arc::from(a.as_slice())),
            ParamValue::Float32Array(a) => Self::Float32Array(Arc::from(a.as_slice())),
            ParamValue::Float64Array(a) => Self::Float64Array(Arc::from(a.as_slice())),
            ParamValue::Enum { index, choices } => Self::Enum {
                index: *index,
                choices: Arc::from(
                    choices
                        .iter()
                        .map(param::EnumEntry::from)
                        .collect::<Vec<_>>(),
                ),
            },
            ParamValue::Undefined => Self::Undefined,
        }
    }
}

impl From<ParamValue> for param::ParamValue {
    fn from(v: ParamValue) -> Self {
        Self::from(&v)
    }
}

/// Optional alarm metadata envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlarmMeta {
    pub status: u16,
    pub severity: u16,
}

/// Timestamp as microseconds since UNIX epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timestamp(pub i64);

impl From<std::time::SystemTime> for Timestamp {
    fn from(t: std::time::SystemTime) -> Self {
        let dur = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        Self(dur.as_micros() as i64)
    }
}

impl From<Timestamp> for std::time::SystemTime {
    fn from(t: Timestamp) -> Self {
        let micros = t.0.max(0) as u64;
        std::time::UNIX_EPOCH + std::time::Duration::from_micros(micros)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_value_int32_serde() {
        let v = ParamValue::Int32(42);
        let json = serde_json::to_string(&v).unwrap();
        let back: ParamValue = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn param_value_all_variants_serde() {
        let variants = vec![
            ParamValue::Int32(-1),
            ParamValue::Int64(i64::MAX),
            ParamValue::Float64(3.14),
            ParamValue::Octet("hello".into()),
            ParamValue::UInt32Digital(0xDEAD),
            ParamValue::Int8Array(vec![1, -1]),
            ParamValue::Int16Array(vec![100, -200]),
            ParamValue::Int32Array(vec![1, 2, 3]),
            ParamValue::Int64Array(vec![i64::MIN, 0]),
            ParamValue::Float32Array(vec![1.0, 2.5]),
            ParamValue::Float64Array(vec![1.0, 2.0, 3.0]),
            ParamValue::Enum {
                index: 1,
                choices: vec![
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
                ],
            },
            ParamValue::Undefined,
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let back: ParamValue = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn param_value_from_internal_roundtrip() {
        let internal = param::ParamValue::Int32(42);
        let proto = ParamValue::from(&internal);
        let back = param::ParamValue::from(&proto);
        if let param::ParamValue::Int32(n) = back {
            assert_eq!(n, 42);
        } else {
            panic!("expected Int32");
        }
    }

    #[test]
    fn param_value_generic_pointer_becomes_undefined() {
        let internal = param::ParamValue::GenericPointer(Arc::new(42i32));
        let proto = ParamValue::from(&internal);
        assert_eq!(proto, ParamValue::Undefined);
    }

    #[test]
    fn param_value_array_roundtrip() {
        let internal = param::ParamValue::Float64Array(Arc::from(vec![1.0, 2.0, 3.0]));
        let proto = ParamValue::from(&internal);
        assert_eq!(proto, ParamValue::Float64Array(vec![1.0, 2.0, 3.0]));
        let back = param::ParamValue::from(&proto);
        if let param::ParamValue::Float64Array(arr) = back {
            assert_eq!(&*arr, &[1.0, 2.0, 3.0]);
        } else {
            panic!("expected Float64Array");
        }
    }

    #[test]
    fn param_value_enum_roundtrip() {
        let entries: Arc<[param::EnumEntry]> = Arc::from(vec![
            param::EnumEntry {
                string: "A".into(),
                value: 0,
                severity: 0,
            },
            param::EnumEntry {
                string: "B".into(),
                value: 1,
                severity: 1,
            },
        ]);
        let internal = param::ParamValue::Enum {
            index: 1,
            choices: entries,
        };
        let proto = ParamValue::from(&internal);
        if let ParamValue::Enum { index, choices } = &proto {
            assert_eq!(*index, 1);
            assert_eq!(choices.len(), 2);
            assert_eq!(choices[0].string, "A");
            assert_eq!(choices[1].severity, 1);
        } else {
            panic!("expected Enum");
        }
        let back = param::ParamValue::from(&proto);
        if let param::ParamValue::Enum { index, choices } = back {
            assert_eq!(index, 1);
            assert_eq!(choices.len(), 2);
        } else {
            panic!("expected Enum");
        }
    }

    #[test]
    fn timestamp_roundtrip() {
        let now = std::time::SystemTime::now();
        let ts = Timestamp::from(now);
        let back = std::time::SystemTime::from(ts);
        // Precision to microsecond
        let diff = now.duration_since(back).unwrap_or_default()
            + back.duration_since(now).unwrap_or_default();
        assert!(diff.as_micros() <= 1);
    }

    #[test]
    fn timestamp_serde() {
        let ts = Timestamp(1_000_000);
        let json = serde_json::to_string(&ts).unwrap();
        let back: Timestamp = serde_json::from_str(&json).unwrap();
        assert_eq!(ts, back);
    }

    #[test]
    fn alarm_meta_serde() {
        let am = AlarmMeta {
            status: 1,
            severity: 2,
        };
        let json = serde_json::to_string(&am).unwrap();
        let back: AlarmMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(am, back);
    }
}
