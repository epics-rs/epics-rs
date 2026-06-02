//! Shared NormativeTypes meta sub-structures: `alarm_t` and `time_t`.
//!
//! Used by NTScalar, NTEnum, NTTable, NTNDArray. Wire IDs match
//! pvxs nt.cpp `Alarm::build()` / `TimeStamp::build()`.

use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};

/// `alarm_t` field descriptor (severity, status, message).
pub fn alarm_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "alarm_t".into(),
        fields: vec![
            ("severity".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("status".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("message".into(), FieldDesc::Scalar(ScalarType::String)),
        ],
    }
}

/// Default `alarm_t` value (all zeros, empty message).
pub fn alarm_default() -> PvField {
    let mut s = PvStructure::new("alarm_t");
    s.fields
        .push(("severity".into(), PvField::Scalar(ScalarValue::Int(0))));
    s.fields
        .push(("status".into(), PvField::Scalar(ScalarValue::Int(0))));
    s.fields.push((
        "message".into(),
        PvField::Scalar(ScalarValue::String(String::new().into())),
    ));
    PvField::Structure(s)
}

/// `time_t` field descriptor (secondsPastEpoch, nanoseconds, userTag).
pub fn time_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "time_t".into(),
        fields: vec![
            (
                "secondsPastEpoch".into(),
                FieldDesc::Scalar(ScalarType::Long),
            ),
            ("nanoseconds".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("userTag".into(), FieldDesc::Scalar(ScalarType::Int)),
        ],
    }
}

/// Default `time_t` value (epoch zero).
pub fn time_default() -> PvField {
    let mut s = PvStructure::new("time_t");
    s.fields.push((
        "secondsPastEpoch".into(),
        PvField::Scalar(ScalarValue::Long(0)),
    ));
    s.fields
        .push(("nanoseconds".into(), PvField::Scalar(ScalarValue::Int(0))));
    s.fields
        .push(("userTag".into(), PvField::Scalar(ScalarValue::Int(0))));
    PvField::Structure(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alarm_descriptor_has_three_fields() {
        if let FieldDesc::Structure { fields, struct_id } = alarm_desc() {
            assert_eq!(struct_id, "alarm_t");
            assert_eq!(fields.len(), 3);
        } else {
            panic!("expected struct");
        }
    }

    #[test]
    fn time_descriptor_has_three_fields() {
        if let FieldDesc::Structure { fields, struct_id } = time_desc() {
            assert_eq!(struct_id, "time_t");
            assert_eq!(fields.len(), 3);
        } else {
            panic!("expected struct");
        }
    }
}
