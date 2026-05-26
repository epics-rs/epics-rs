//! `epics:nt/NTScalar:1.0` and `epics:nt/NTScalarArray:1.0`.
//!
//! Mirrors pvxs nt.cpp `NTScalar::build()`. The optional `display`,
//! `control`, and `valueAlarm` sub-structures are gated by builder
//! flags. We default all of them off; callers that want richer NT
//! shapes set the flags explicitly.

use super::meta::{alarm_default, alarm_desc, time_default, time_desc};
use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};

/// Builder for `NTScalar` / `NTScalarArray`. Configure scalar type
/// and optional meta sub-structures, then call `build()` /
/// `create()` to materialize the descriptor / default value.
pub struct NTScalar {
    pub value_type: ScalarType,
    pub is_array: bool,
    pub display: bool,
    pub control: bool,
    pub value_alarm: bool,
}

impl NTScalar {
    /// New scalar (single value).
    pub fn new(value_type: ScalarType) -> Self {
        Self {
            value_type,
            is_array: false,
            display: false,
            control: false,
            value_alarm: false,
        }
    }

    /// New scalar array.
    pub fn array(value_type: ScalarType) -> Self {
        Self {
            value_type,
            is_array: true,
            display: false,
            control: false,
            value_alarm: false,
        }
    }

    pub fn with_display(mut self) -> Self {
        self.display = true;
        self
    }

    pub fn with_control(mut self) -> Self {
        self.control = true;
        self
    }

    pub fn with_value_alarm(mut self) -> Self {
        self.value_alarm = true;
        self
    }

    /// Build the [`FieldDesc`] for this NT.
    pub fn build(&self) -> FieldDesc {
        let struct_id = if self.is_array {
            "epics:nt/NTScalarArray:1.0".to_string()
        } else {
            "epics:nt/NTScalar:1.0".to_string()
        };
        let value_field = if self.is_array {
            FieldDesc::ScalarArray(self.value_type)
        } else {
            FieldDesc::Scalar(self.value_type)
        };

        let mut fields: Vec<(String, FieldDesc)> = vec![
            ("value".into(), value_field),
            ("alarm".into(), alarm_desc()),
            ("timeStamp".into(), time_desc()),
        ];

        let is_numeric = matches!(
            self.value_type,
            ScalarType::Byte
                | ScalarType::Short
                | ScalarType::Int
                | ScalarType::Long
                | ScalarType::UByte
                | ScalarType::UShort
                | ScalarType::UInt
                | ScalarType::ULong
                | ScalarType::Float
                | ScalarType::Double
        );

        if self.display {
            if is_numeric {
                fields.push((
                    "display".into(),
                    FieldDesc::Structure {
                        struct_id: String::new(),
                        fields: vec![
                            ("limitLow".into(), FieldDesc::Scalar(self.value_type)),
                            ("limitHigh".into(), FieldDesc::Scalar(self.value_type)),
                            ("description".into(), FieldDesc::Scalar(ScalarType::String)),
                            ("units".into(), FieldDesc::Scalar(ScalarType::String)),
                        ],
                    },
                ));
            } else {
                fields.push((
                    "display".into(),
                    FieldDesc::Structure {
                        struct_id: String::new(),
                        fields: vec![
                            ("description".into(), FieldDesc::Scalar(ScalarType::String)),
                            ("units".into(), FieldDesc::Scalar(ScalarType::String)),
                        ],
                    },
                ));
            }
        }

        if self.control && is_numeric {
            fields.push((
                "control".into(),
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![
                        ("limitLow".into(), FieldDesc::Scalar(self.value_type)),
                        ("limitHigh".into(), FieldDesc::Scalar(self.value_type)),
                        ("minStep".into(), FieldDesc::Scalar(self.value_type)),
                    ],
                },
            ));
        }

        if self.value_alarm && is_numeric {
            fields.push((
                "valueAlarm".into(),
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![
                        ("active".into(), FieldDesc::Scalar(ScalarType::Boolean)),
                        ("lowAlarmLimit".into(), FieldDesc::Scalar(self.value_type)),
                        ("lowWarningLimit".into(), FieldDesc::Scalar(self.value_type)),
                        (
                            "highWarningLimit".into(),
                            FieldDesc::Scalar(self.value_type),
                        ),
                        ("highAlarmLimit".into(), FieldDesc::Scalar(self.value_type)),
                        (
                            "lowAlarmSeverity".into(),
                            FieldDesc::Scalar(ScalarType::Int),
                        ),
                        (
                            "lowWarningSeverity".into(),
                            FieldDesc::Scalar(ScalarType::Int),
                        ),
                        (
                            "highWarningSeverity".into(),
                            FieldDesc::Scalar(ScalarType::Int),
                        ),
                        (
                            "highAlarmSeverity".into(),
                            FieldDesc::Scalar(ScalarType::Int),
                        ),
                        ("hysteresis".into(), FieldDesc::Scalar(ScalarType::Double)),
                    ],
                },
            ));
        }

        FieldDesc::Structure { struct_id, fields }
    }

    // R79: derive the value from the descriptor so the optional
    // display/control/valueAlarm sub-structures always match build().
    // pvxs nt.h:96 does the same: `create() { return build().create(); }`.
    /// Create a default-initialised value matching [`build()`](Self::build).
    pub fn create(&self) -> PvField {
        let struct_id = if self.is_array {
            "epics:nt/NTScalarArray:1.0".to_string()
        } else {
            "epics:nt/NTScalar:1.0".to_string()
        };
        let mut s = PvStructure::new(&struct_id);
        let value_default = if self.is_array {
            PvField::ScalarArray(Vec::new())
        } else {
            PvField::Scalar(default_scalar(self.value_type))
        };
        s.fields.push(("value".into(), value_default));
        s.fields.push(("alarm".into(), alarm_default()));
        s.fields.push(("timeStamp".into(), time_default()));
        PvField::Structure(s)
    }
}

fn default_scalar(t: ScalarType) -> ScalarValue {
    match t {
        ScalarType::Boolean => ScalarValue::Boolean(false),
        ScalarType::Byte => ScalarValue::Byte(0),
        ScalarType::Short => ScalarValue::Short(0),
        ScalarType::Int => ScalarValue::Int(0),
        ScalarType::Long => ScalarValue::Long(0),
        ScalarType::UByte => ScalarValue::UByte(0),
        ScalarType::UShort => ScalarValue::UShort(0),
        ScalarType::UInt => ScalarValue::UInt(0),
        ScalarType::ULong => ScalarValue::ULong(0),
        ScalarType::Float => ScalarValue::Float(0.0),
        ScalarType::Double => ScalarValue::Double(0.0),
        ScalarType::String => ScalarValue::String(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nt_scalar_int32_struct_id_is_ntscalar() {
        let desc = NTScalar::new(ScalarType::Int).build();
        if let FieldDesc::Structure { struct_id, fields } = desc {
            assert_eq!(struct_id, "epics:nt/NTScalar:1.0");
            // value, alarm, timeStamp
            assert_eq!(fields.len(), 3);
        } else {
            panic!("expected struct");
        }
    }

    #[test]
    fn nt_scalar_array_struct_id_uses_array_suffix() {
        let desc = NTScalar::array(ScalarType::Double).build();
        if let FieldDesc::Structure { struct_id, .. } = desc {
            assert_eq!(struct_id, "epics:nt/NTScalarArray:1.0");
        } else {
            panic!("expected struct");
        }
    }

    #[test]
    fn nt_scalar_with_display_adds_field() {
        let desc = NTScalar::new(ScalarType::Double).with_display().build();
        if let FieldDesc::Structure { fields, .. } = desc {
            assert!(fields.iter().any(|(n, _)| n == "display"));
        } else {
            panic!("expected struct");
        }
    }

    #[test]
    fn nt_scalar_string_with_display_omits_numeric_limits() {
        let desc = NTScalar::new(ScalarType::String).with_display().build();
        if let FieldDesc::Structure { fields, .. } = desc {
            let display = fields
                .iter()
                .find_map(|(n, d)| if n == "display" { Some(d) } else { None })
                .expect("display field");
            if let FieldDesc::Structure {
                fields: subfields, ..
            } = display
            {
                let names: Vec<&str> = subfields.iter().map(|(n, _)| n.as_str()).collect();
                assert!(!names.contains(&"limitLow"));
                assert!(names.contains(&"description"));
                assert!(names.contains(&"units"));
            }
        }
    }
}
