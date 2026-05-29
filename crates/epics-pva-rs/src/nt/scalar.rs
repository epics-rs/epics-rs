//! `epics:nt/NTScalar:1.0` and `epics:nt/NTScalarArray:1.0`.
//!
//! Mirrors pvxs nt.cpp `NTScalar::build()`. The optional `display`,
//! `control`, and `valueAlarm` sub-structures are gated by builder
//! flags. We default all of them off; callers that want richer NT
//! shapes set the flags explicitly.

use super::meta::{alarm_desc, time_desc};
use crate::pvdata::{FieldDesc, PvField, ScalarType};

/// Builder for `NTScalar` / `NTScalarArray`. Configure scalar type
/// and optional meta sub-structures, then call `build()` /
/// `create()` to materialize the descriptor / default value.
pub struct NTScalar {
    pub value_type: ScalarType,
    pub is_array: bool,
    pub display: bool,
    pub control: bool,
    pub value_alarm: bool,
    /// pvxs `NTScalar::form` (`src/pvxs/nt.h:68-89`): when set, the numeric
    /// `display` structure also carries `display.precision` and an
    /// `display.form` `enum_t`. pvxs gates this inside `if(display &&
    /// isnumeric)`, so `form` only takes effect together with `display`.
    pub form: bool,
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
            form: false,
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
            form: false,
        }
    }

    pub fn with_display(mut self) -> Self {
        self.display = true;
        self
    }

    /// Add `display.precision` + `display.form` (`enum_t`) to the numeric
    /// display structure, mirroring pvxs `NTScalar{ ..., form=true }`
    /// (`src/nt.cpp:67-77`). pvxs emits these only when `display` is also
    /// set, so this implies [`with_display`](Self::with_display).
    pub fn with_form(mut self) -> Self {
        self.form = true;
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
                // pvxs `nt.cpp:58-66` numeric display base.
                let mut display_fields = vec![
                    ("limitLow".into(), FieldDesc::Scalar(self.value_type)),
                    ("limitHigh".into(), FieldDesc::Scalar(self.value_type)),
                    ("description".into(), FieldDesc::Scalar(ScalarType::String)),
                    ("units".into(), FieldDesc::Scalar(ScalarType::String)),
                ];
                if self.form {
                    // pvxs `nt.cpp:67-77` merges a second `Struct("display",
                    // {precision, form enum_t})` into the same display
                    // struct; the Rust builder appends those members
                    // directly (final order: limits, description, units,
                    // precision, form).
                    display_fields.push(("precision".into(), FieldDesc::Scalar(ScalarType::Int)));
                    display_fields.push((
                        "form".into(),
                        FieldDesc::Structure {
                            struct_id: "enum_t".into(),
                            fields: vec![
                                ("index".into(), FieldDesc::Scalar(ScalarType::Int)),
                                ("choices".into(), FieldDesc::ScalarArray(ScalarType::String)),
                            ],
                        },
                    ));
                }
                fields.push((
                    "display".into(),
                    FieldDesc::Structure {
                        struct_id: String::new(),
                        fields: display_fields,
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

    // derive the value from the descriptor so the optional
    // display/control/valueAlarm sub-structures always match build().
    // pvxs nt.h:96 does the same: `create() { return build().create(); }`.
    /// Create a default-initialised value matching [`build()`](Self::build).
    pub fn create(&self) -> PvField {
        crate::pvdata::encode::default_value_for(&self.build())
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
    fn nt_scalar_with_form_adds_precision_and_form_enum_to_display() {
        // pvxs `nt.cpp:67-77`: a numeric NTScalar with `form` carries
        // `display.precision` (Int32) and `display.form` (enum_t).
        let desc = NTScalar::new(ScalarType::Double).with_form().build();
        let FieldDesc::Structure { fields, .. } = desc else {
            panic!("expected struct");
        };
        let display = fields
            .iter()
            .find_map(|(n, d)| (n == "display").then_some(d))
            .expect("display field");
        let FieldDesc::Structure {
            fields: subfields, ..
        } = display
        else {
            panic!("display must be a structure");
        };
        // Order matches pvxs merge: limits, description, units, precision, form.
        let names: Vec<&str> = subfields.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "limitLow",
                "limitHigh",
                "description",
                "units",
                "precision",
                "form"
            ]
        );
        // precision is Int32.
        let precision = &subfields.iter().find(|(n, _)| n == "precision").unwrap().1;
        assert!(matches!(precision, FieldDesc::Scalar(ScalarType::Int)));
        // form is an enum_t { index: Int32, choices: String[] }.
        let form = &subfields.iter().find(|(n, _)| n == "form").unwrap().1;
        let FieldDesc::Structure {
            struct_id: form_id,
            fields: form_fields,
        } = form
        else {
            panic!("form must be a structure");
        };
        assert_eq!(form_id, "enum_t");
        assert!(matches!(
            form_fields.iter().find(|(n, _)| n == "index").unwrap().1,
            FieldDesc::Scalar(ScalarType::Int)
        ));
        assert!(matches!(
            form_fields.iter().find(|(n, _)| n == "choices").unwrap().1,
            FieldDesc::ScalarArray(ScalarType::String)
        ));
    }

    #[test]
    fn nt_scalar_form_without_display_still_emits_display() {
        // `with_form()` implies `with_display()` (pvxs gates form inside
        // `if(display)`), so the display structure is present.
        let desc = NTScalar::new(ScalarType::Int).with_form().build();
        let FieldDesc::Structure { fields, .. } = desc else {
            panic!("expected struct");
        };
        assert!(fields.iter().any(|(n, _)| n == "display"));
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
