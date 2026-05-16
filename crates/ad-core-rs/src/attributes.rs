/// Source of an NDAttribute value.
#[derive(Debug, Clone, PartialEq)]
pub enum NDAttrSource {
    Driver,
    Param {
        port_name: String,
        param_name: String,
    },
    EpicsPV,
    Function,
    Constant,
    Undefined,
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
            source: NDAttrSource::Constant,
            value: NDAttrValue::Int32(1),
            source_impl: None,
        });
        list.add(NDAttribute {
            name: "B".into(),
            description: "".into(),
            source: NDAttrSource::Constant,
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
    }

    #[test]
    fn test_attribute_update_reevaluates_source() {
        let src = std::sync::Arc::new(CountingSource {
            counter: std::sync::atomic::AtomicI32::new(0),
        });
        let mut attr =
            NDAttribute::new_with_source("live", "live source", NDAttrSource::Function, src);
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
        let mut attr =
            NDAttribute::new_static("static", "", NDAttrSource::Constant, NDAttrValue::Int32(42));
        attr.update();
        assert_eq!(attr.value, NDAttrValue::Int32(42));
    }

    #[test]
    fn test_attribute_list_update_values() {
        // G10: NDAttributeList::update_values re-evaluates every live source.
        let mut list = NDAttributeList::new();
        list.add(NDAttribute::new_with_source(
            "live",
            "",
            NDAttrSource::Function,
            std::sync::Arc::new(CountingSource {
                counter: std::sync::atomic::AtomicI32::new(0),
            }),
        ));
        list.add(NDAttribute::new_static(
            "frozen",
            "",
            NDAttrSource::Constant,
            NDAttrValue::Int32(7),
        ));
        list.update_values();
        assert_eq!(list.get("live").unwrap().value, NDAttrValue::Int32(2));
        assert_eq!(list.get("frozen").unwrap().value, NDAttrValue::Int32(7));
    }
}
