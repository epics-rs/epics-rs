//! `epics:nt/NTEnum:1.0` builder.
//!
//! Mirrors pvxs nt.cpp `NTEnum::build()`.

use super::meta::{alarm_default, alarm_desc, time_default, time_desc};
use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};

/// Builder for `NTEnum`. Default initialised with empty choices list.
pub struct NTEnum {
    pub choices: Vec<String>,
}

impl NTEnum {
    pub fn new() -> Self {
        Self {
            choices: Vec::new(),
        }
    }

    pub fn with_choices<I, S>(mut self, choices: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.choices = choices.into_iter().map(Into::into).collect();
        self
    }

    pub fn build(&self) -> FieldDesc {
        FieldDesc::Structure {
            struct_id: "epics:nt/NTEnum:1.0".into(),
            fields: vec![
                (
                    "value".into(),
                    FieldDesc::Structure {
                        struct_id: "enum_t".into(),
                        fields: vec![
                            ("index".into(), FieldDesc::Scalar(ScalarType::Int)),
                            ("choices".into(), FieldDesc::ScalarArray(ScalarType::String)),
                        ],
                    },
                ),
                ("alarm".into(), alarm_desc()),
                ("timeStamp".into(), time_desc()),
                (
                    "display".into(),
                    FieldDesc::Structure {
                        struct_id: String::new(),
                        fields: vec![("description".into(), FieldDesc::Scalar(ScalarType::String))],
                    },
                ),
            ],
        }
    }

    pub fn create(&self) -> PvField {
        let mut root = PvStructure::new("epics:nt/NTEnum:1.0");
        let mut value = PvStructure::new("enum_t");
        value
            .fields
            .push(("index".into(), PvField::Scalar(ScalarValue::Int(0))));
        let choices_arr = self
            .choices
            .iter()
            .map(|s| ScalarValue::String(s.clone().into()))
            .collect::<Vec<_>>();
        value
            .fields
            .push(("choices".into(), PvField::ScalarArray(choices_arr)));
        root.fields
            .push(("value".into(), PvField::Structure(value)));
        root.fields.push(("alarm".into(), alarm_default()));
        root.fields.push(("timeStamp".into(), time_default()));
        let mut display = PvStructure::new("");
        display.fields.push((
            "description".into(),
            PvField::Scalar(ScalarValue::String(String::new().into())),
        ));
        root.fields
            .push(("display".into(), PvField::Structure(display)));
        PvField::Structure(root)
    }
}

impl Default for NTEnum {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nt_enum_struct_id() {
        let desc = NTEnum::new().build();
        if let FieldDesc::Structure { struct_id, .. } = desc {
            assert_eq!(struct_id, "epics:nt/NTEnum:1.0");
        } else {
            panic!("expected struct");
        }
    }

    #[test]
    fn nt_enum_with_choices_populates_value() {
        let v = NTEnum::new().with_choices(["off", "on", "fault"]).create();
        if let PvField::Structure(root) = v {
            if let Some(PvField::Structure(value)) = root.get_field("value") {
                if let Some(PvField::ScalarArray(arr)) = value.get_field("choices") {
                    assert_eq!(arr.len(), 3);
                }
            }
        }
    }
}
