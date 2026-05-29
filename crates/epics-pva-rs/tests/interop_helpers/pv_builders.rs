//! Shared PV-shape builders used by both the live-pvxs interop
//! matrix and the default-suite wire-golden replay. Keeping the
//! constructors in one place is the contract that makes the
//! capture/replay loop correct: a regression in either Rust
//! encoder or PV definition would show up as a fixture mismatch
//! against the goldens captured during the last interop run.

#![allow(dead_code)]

use epics_pva_rs::nt::nd_array::{
    NdAlarm, NdArrayBuffer, NdAttribute, NdCodec, NdDimension, NdTimeStamp, NtNdArray,
    nt_nd_array_desc, nt_nd_array_value,
};
use epics_pva_rs::nt::{NTEnum, NTScalar, NTTable, meta};
use epics_pva_rs::pvdata::{
    FieldDesc, PvField, PvStructure, ScalarType, ScalarValue, VariantValue,
};
use epics_pva_rs::server_native::SharedPV;

#[derive(Clone)]
pub struct PvBuild {
    pub name: &'static str,
    pub desc: FieldDesc,
    pub value: PvField,
}

impl PvBuild {
    pub fn open(&self) -> SharedPV {
        let pv = SharedPV::new();
        pv.open(self.desc.clone(), self.value.clone())
            .expect("fresh PvBuild PV opens");
        pv
    }
}

fn nt_scalar_struct(t: ScalarType, value: ScalarValue) -> (FieldDesc, PvField) {
    let desc = NTScalar::new(t).build();
    let mut root = PvStructure::new("epics:nt/NTScalar:1.0");
    root.fields
        .push(("value".to_string(), PvField::Scalar(value)));
    root.fields
        .push(("alarm".to_string(), meta::alarm_default()));
    root.fields
        .push(("timeStamp".to_string(), meta::time_default()));
    (desc, PvField::Structure(root))
}

fn nt_scalar_array_struct(t: ScalarType, value: Vec<ScalarValue>) -> (FieldDesc, PvField) {
    let desc = NTScalar::array(t).build();
    let mut root = PvStructure::new("epics:nt/NTScalarArray:1.0");
    root.fields
        .push(("value".to_string(), PvField::ScalarArray(value)));
    root.fields
        .push(("alarm".to_string(), meta::alarm_default()));
    root.fields
        .push(("timeStamp".to_string(), meta::time_default()));
    (desc, PvField::Structure(root))
}

fn nt_enum_struct(index: i32, choices: &[&str]) -> (FieldDesc, PvField) {
    // Use the builder's `create()` so the descriptor and value
    // agree on field layout (the descriptor has a trailing
    // `display.description` field that the value must populate
    // too — encoding a value short of the descriptor leaves the
    // tail bytes default-initialised on the wire and breaks
    // re-decode equality).
    let builder = NTEnum::new().with_choices(choices.iter().copied());
    let desc = builder.build();
    let mut value = builder.create();
    if let PvField::Structure(ref mut root) = value
        && let Some((_, PvField::Structure(inner))) =
            root.fields.iter_mut().find(|(n, _)| n == "value")
        && let Some((_, PvField::Scalar(ScalarValue::Int(idx)))) =
            inner.fields.iter_mut().find(|(n, _)| n == "index")
    {
        *idx = index;
    }
    (desc, value)
}

fn nt_table_struct() -> (FieldDesc, PvField) {
    let t = NTTable::new()
        .add_column(ScalarType::Double, "xs", Some("X axis"))
        .add_column(ScalarType::Double, "ys", Some("Y axis"))
        .add_column(ScalarType::String, "name", Some("Name"));
    let desc = t.build();
    let mut root = PvStructure::new("epics:nt/NTTable:1.0");
    root.fields.push((
        "labels".to_string(),
        PvField::ScalarArray(vec![
            ScalarValue::String("X axis".into()),
            ScalarValue::String("Y axis".into()),
            ScalarValue::String("Name".into()),
        ]),
    ));
    let mut cols = PvStructure::new("");
    cols.fields.push((
        "xs".to_string(),
        PvField::ScalarArray(vec![
            ScalarValue::Double(1.0),
            ScalarValue::Double(2.0),
            ScalarValue::Double(3.0),
        ]),
    ));
    cols.fields.push((
        "ys".to_string(),
        PvField::ScalarArray(vec![
            ScalarValue::Double(10.0),
            ScalarValue::Double(20.0),
            ScalarValue::Double(30.0),
        ]),
    ));
    cols.fields.push((
        "name".to_string(),
        PvField::ScalarArray(vec![
            ScalarValue::String("a".into()),
            ScalarValue::String("b".into()),
            ScalarValue::String("c".into()),
        ]),
    ));
    root.fields
        .push(("value".to_string(), PvField::Structure(cols)));
    root.fields.push((
        "descriptor".to_string(),
        PvField::Scalar(ScalarValue::String("table".into())),
    ));
    root.fields
        .push(("alarm".to_string(), meta::alarm_default()));
    root.fields
        .push(("timeStamp".to_string(), meta::time_default()));
    (desc, PvField::Structure(root))
}

/// Structure array of `point_t { x: int, y: string }` with 3 elements.
/// Exercises the `0x88` StructureArray wire tag and per-element
/// non-null marker (`PvField::StructureArray` is `Vec<PvStructure>`,
/// each element rendered inline with the descriptor's struct shape).
fn struct_array_points() -> (FieldDesc, PvField) {
    let elem_desc = FieldDesc::Structure {
        struct_id: "point_t".into(),
        fields: vec![
            ("x".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("y".into(), FieldDesc::Scalar(ScalarType::String)),
        ],
    };
    let desc = FieldDesc::Structure {
        struct_id: "test:points:1.0".into(),
        fields: vec![(
            "points".into(),
            FieldDesc::StructureArray {
                struct_id: "point_t".into(),
                fields: match &elem_desc {
                    FieldDesc::Structure { fields, .. } => fields.clone(),
                    _ => unreachable!(),
                },
            },
        )],
    };
    let make = |x: i32, y: &str| PvStructure {
        struct_id: "point_t".into(),
        fields: vec![
            ("x".into(), PvField::Scalar(ScalarValue::Int(x))),
            (
                "y".into(),
                PvField::Scalar(ScalarValue::String(y.to_string())),
            ),
        ],
    };
    let root = PvField::Structure(PvStructure {
        struct_id: "test:points:1.0".into(),
        fields: vec![(
            "points".into(),
            PvField::StructureArray(vec![
                Some(make(1, "alpha")),
                Some(make(2, "beta")),
                Some(make(3, "gamma")),
            ]),
        )],
    });
    (desc, root)
}

/// Variant ("Any") field with a Scalar(Int) payload. pvxs wire tag
/// 0x82 followed by the inner descriptor + value.
fn variant_int() -> (FieldDesc, PvField) {
    let desc = FieldDesc::Structure {
        struct_id: "test:variant:1.0".into(),
        fields: vec![("any".into(), FieldDesc::Variant)],
    };
    let root = PvField::Structure(PvStructure {
        struct_id: "test:variant:1.0".into(),
        fields: vec![(
            "any".into(),
            PvField::Variant(Box::new(VariantValue {
                desc: Some(FieldDesc::Scalar(ScalarType::Int)),
                value: PvField::Scalar(ScalarValue::Int(424242)),
            })),
        )],
    });
    (desc, root)
}

/// NTNDArray with a tiny 4-element ubyte image. Exercises Union
/// (value branch select), nested Structure (codec, alarm,
/// timeStamp, dataTimeStamp), StructureArray (dimension), and
/// StructureArray with inner Variant (attribute).
fn nt_nd_array_4byte() -> (FieldDesc, PvField) {
    let nt = NtNdArray {
        value: NdArrayBuffer::UByte((0u8..4).collect()),
        codec: NdCodec::default(),
        compressed_size: 4,
        uncompressed_size: 4,
        unique_id: 7,
        data_time_stamp: NdTimeStamp::default(),
        alarm: NdAlarm {
            message: "NO_ALARM".into(),
            ..NdAlarm::default()
        },
        time_stamp: NdTimeStamp::default(),
        dimension: vec![NdDimension {
            size: 4,
            full_size: 4,
            binning: 1,
            ..NdDimension::default()
        }],
        attribute: vec![NdAttribute {
            name: "ColorMode".into(),
            value: VariantValue::scalar(ScalarValue::Int(0)),
            descriptor: "Mono".into(),
            source: "driver".into(),
            ..NdAttribute::default()
        }],
    };
    (nt_nd_array_desc(), nt_nd_array_value(&nt))
}

fn nested_struct() -> (FieldDesc, PvField) {
    let desc = FieldDesc::Structure {
        struct_id: "test:nested:1.0".into(),
        fields: vec![
            (
                "outer".into(),
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![
                        (
                            "mid".into(),
                            FieldDesc::Structure {
                                struct_id: String::new(),
                                fields: vec![
                                    ("count".into(), FieldDesc::Scalar(ScalarType::Long)),
                                    ("label".into(), FieldDesc::Scalar(ScalarType::String)),
                                ],
                            },
                        ),
                        ("flag".into(), FieldDesc::Scalar(ScalarType::Boolean)),
                    ],
                },
            ),
            ("tags".into(), FieldDesc::ScalarArray(ScalarType::String)),
        ],
    };
    let inner = PvField::Structure(PvStructure {
        struct_id: String::new(),
        fields: vec![
            (
                "count".to_string(),
                PvField::Scalar(ScalarValue::Long(987_654_321_i64)),
            ),
            (
                "label".to_string(),
                PvField::Scalar(ScalarValue::String("nested-leaf".into())),
            ),
        ],
    });
    let outer = PvField::Structure(PvStructure {
        struct_id: String::new(),
        fields: vec![
            ("mid".to_string(), inner),
            (
                "flag".to_string(),
                PvField::Scalar(ScalarValue::Boolean(true)),
            ),
        ],
    });
    let root = PvField::Structure(PvStructure {
        struct_id: "test:nested:1.0".into(),
        fields: vec![
            ("outer".to_string(), outer),
            (
                "tags".to_string(),
                PvField::ScalarArray(vec![
                    ScalarValue::String("alpha".into()),
                    ScalarValue::String("beta".into()),
                ]),
            ),
        ],
    });
    (desc, root)
}

/// Canonical matrix of complex-type PVs. Both the live-pvxs interop
/// matrix and the default-suite golden replay iterate this list,
/// so additions show up in both checks automatically.
pub fn complex_pv_matrix() -> Vec<PvBuild> {
    let mut out = Vec::new();
    let mut push = |name, (desc, value)| out.push(PvBuild { name, desc, value });

    push(
        "T:STR",
        nt_scalar_struct(
            ScalarType::String,
            ScalarValue::String("hello world".into()),
        ),
    );
    push(
        "T:INT",
        nt_scalar_struct(ScalarType::Int, ScalarValue::Int(-12345)),
    );
    push(
        "T:LONG",
        nt_scalar_struct(ScalarType::Long, ScalarValue::Long(9_000_000_000_i64)),
    );
    push(
        "T:DBL",
        nt_scalar_struct(ScalarType::Double, ScalarValue::Double(123.456_789_f64)),
    );
    push(
        "T:WF:DBL",
        nt_scalar_array_struct(
            ScalarType::Double,
            vec![
                ScalarValue::Double(1.5),
                ScalarValue::Double(2.5),
                ScalarValue::Double(3.5),
            ],
        ),
    );
    push(
        "T:WF:INT",
        nt_scalar_array_struct(
            ScalarType::Int,
            vec![
                ScalarValue::Int(7),
                ScalarValue::Int(8),
                ScalarValue::Int(9),
                ScalarValue::Int(10),
            ],
        ),
    );
    push(
        "T:WF:STR",
        nt_scalar_array_struct(
            ScalarType::String,
            vec![
                ScalarValue::String("alpha".into()),
                ScalarValue::String("beta".into()),
                ScalarValue::String("gamma".into()),
            ],
        ),
    );
    push("T:ENUM", nt_enum_struct(2, &["OFF", "ON", "AUTO"]));
    push("T:TBL", nt_table_struct());
    push("T:NEST", nested_struct());
    push("T:SA", struct_array_points());
    push("T:ANY", variant_int());
    push("T:NDARR", nt_nd_array_4byte());

    out
}

/// Encode a PV's descriptor + value to flat bytes, with a small
/// length-prefixed framing so a single fixture file holds both
/// halves and can be split deterministically on replay.
///
/// Layout:
///   `u32_le(desc_len) | desc_bytes | u32_le(value_len) | value_bytes`
///
/// Both halves are LE-encoded (matches the default the Rust server
/// negotiates locally; the matrix test runs against the loopback
/// server which uses LE). The framing is a fixture-only convention
/// — it never appears on the wire.
pub fn encode_pv_fixture(build: &PvBuild) -> Vec<u8> {
    use epics_pva_rs::proto::ByteOrder;
    use epics_pva_rs::pvdata::encode::{encode_pv_field, encode_type_desc};

    let mut desc_bytes = Vec::new();
    encode_type_desc(&build.desc, ByteOrder::Little, &mut desc_bytes);
    let mut value_bytes = Vec::new();
    encode_pv_field(
        &build.value,
        &build.desc,
        ByteOrder::Little,
        &mut value_bytes,
    );

    let mut out = Vec::with_capacity(8 + desc_bytes.len() + value_bytes.len());
    out.extend_from_slice(&(desc_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&desc_bytes);
    out.extend_from_slice(&(value_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&value_bytes);
    out
}

/// Reverse of `encode_pv_fixture`: split a fixture file into its
/// descriptor and value byte halves.
pub fn split_fixture(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    if bytes.len() < 4 {
        return None;
    }
    let desc_len = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
    if bytes.len() < 4 + desc_len + 4 {
        return None;
    }
    let desc = &bytes[4..4 + desc_len];
    let off = 4 + desc_len;
    let val_len = u32::from_le_bytes(bytes[off..off + 4].try_into().ok()?) as usize;
    if bytes.len() < off + 4 + val_len {
        return None;
    }
    let value = &bytes[off + 4..off + 4 + val_len];
    Some((desc, value))
}
