//! `epics:nt/NTAttribute:1.0` builder.
//!
//! pvxs `nt.cpp:238` defines NTAttribute with the fields `name`,
//! `value` (Variant), `tags`, `descriptor`, `alarm`, `timeStamp`,
//! `sourceType`, `source`. This builder emits that full 8-field shape.
//! The NTNDArray `attribute[]` element type is built separately in
//! `nd_array.rs::attribute_desc()` (also `:1.0`).

use super::meta::{alarm_default, alarm_desc, time_default, time_desc};
use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue, VariantValue};

/// Builder for a single `NTAttribute` instance.
pub struct NTAttribute {
    pub name: String,
    /// Carried value as a Variant. `None` = null.
    pub value: Option<(FieldDesc, PvField)>,
    pub descriptor: String,
}

impl NTAttribute {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: None,
            descriptor: String::new(),
        }
    }

    pub fn with_value(mut self, desc: FieldDesc, value: PvField) -> Self {
        self.value = Some((desc, value));
        self
    }

    pub fn with_descriptor(mut self, desc: impl Into<String>) -> Self {
        self.descriptor = desc.into();
        self
    }

    /// FieldDesc for an NTAttribute element (used as the element type of
    /// NTNDArray's `attribute[]`).
    pub fn build() -> FieldDesc {
        FieldDesc::Structure {
            // R81: pvxs defines only `epics:nt/NTAttribute:1.0` (nt.cpp:238);
            // `:1.1` does not exist upstream and disagrees with the NTNDArray
            // embed in nd_array.rs.
            struct_id: "epics:nt/NTAttribute:1.0".into(),
            fields: vec![
                ("name".into(), FieldDesc::Scalar(ScalarType::String)),
                ("value".into(), FieldDesc::Variant),
                ("tags".into(), FieldDesc::ScalarArray(ScalarType::String)),
                ("descriptor".into(), FieldDesc::Scalar(ScalarType::String)),
                ("alarm".into(), alarm_desc()),
                ("timeStamp".into(), time_desc()),
                ("sourceType".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("source".into(), FieldDesc::Scalar(ScalarType::String)),
            ],
        }
    }

    pub fn create(&self) -> PvField {
        let mut s = PvStructure::new("epics:nt/NTAttribute:1.0");
        s.fields.push((
            "name".into(),
            PvField::Scalar(ScalarValue::String(self.name.clone())),
        ));
        let variant = match &self.value {
            Some((d, v)) => VariantValue {
                desc: Some(d.clone()),
                value: v.clone(),
            },
            None => VariantValue {
                desc: None,
                value: PvField::Null,
            },
        };
        s.fields
            .push(("value".into(), PvField::Variant(Box::new(variant))));
        s.fields
            .push(("tags".into(), PvField::ScalarArray(Vec::new())));
        s.fields.push((
            "descriptor".into(),
            PvField::Scalar(ScalarValue::String(self.descriptor.clone())),
        ));
        s.fields.push(("alarm".into(), alarm_default()));
        s.fields.push(("timeStamp".into(), time_default()));
        s.fields
            .push(("sourceType".into(), PvField::Scalar(ScalarValue::Int(0))));
        s.fields.push((
            "source".into(),
            PvField::Scalar(ScalarValue::String(String::new())),
        ));
        PvField::Structure(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nt_attribute_struct_id_and_fields() {
        if let FieldDesc::Structure { struct_id, fields } = NTAttribute::build() {
            assert_eq!(struct_id, "epics:nt/NTAttribute:1.0");
            let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
            assert!(names.contains(&"name"));
            assert!(names.contains(&"value"));
            assert!(names.contains(&"descriptor"));
        }
    }

    #[test]
    fn nt_attribute_create_with_value() {
        let a = NTAttribute::new("Gain")
            .with_value(
                FieldDesc::Scalar(ScalarType::Double),
                PvField::Scalar(ScalarValue::Double(2.5)),
            )
            .with_descriptor("Detector gain");
        let v = a.create();
        if let PvField::Structure(s) = v {
            assert_eq!(
                s.get_field("name"),
                Some(&PvField::Scalar(ScalarValue::String("Gain".into())))
            );
        }
    }
}
