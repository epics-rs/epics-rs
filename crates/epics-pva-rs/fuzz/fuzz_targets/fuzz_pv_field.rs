#![no_main]
//! Fuzz `decode_pv_field` against a synthetic introspection. The
//! target descriptor is a fully-typed NTScalar struct so the decoder
//! exercises the full `Status + bitset + value` GET response path —
//! the most common shape on the wire. Fuzzed bytes flow into the
//! value position; the fuzzer drives bitset values, scalar payloads,
//! and trailing-bytes-too-short scenarios.

use std::io::Cursor;
use std::sync::OnceLock;

use epics_pva_rs::proto::ByteOrder;
use epics_pva_rs::pvdata::encode::decode_pv_field;
use epics_pva_rs::pvdata::{FieldDesc, ScalarType};
use libfuzzer_sys::fuzz_target;

fn intro() -> &'static FieldDesc {
    static D: OnceLock<FieldDesc> = OnceLock::new();
    D.get_or_init(|| FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields: vec![
            ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
            (
                "alarm".into(),
                FieldDesc::Structure {
                    struct_id: "alarm_t".into(),
                    fields: vec![
                        ("severity".into(), FieldDesc::Scalar(ScalarType::Int)),
                        ("status".into(), FieldDesc::Scalar(ScalarType::Int)),
                        ("message".into(), FieldDesc::Scalar(ScalarType::String)),
                    ]
                    .into(),
                },
            ),
            (
                "timeStamp".into(),
                FieldDesc::Structure {
                    struct_id: "time_t".into(),
                    fields: vec![
                        ("secondsPastEpoch".into(), FieldDesc::Scalar(ScalarType::Long)),
                        ("nanoseconds".into(), FieldDesc::Scalar(ScalarType::UInt)),
                        ("userTag".into(), FieldDesc::Scalar(ScalarType::Int)),
                    ]
                    .into(),
                },
            ),
        ]
        .into(),
    })
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let order = if data[0] & 1 == 0 {
        ByteOrder::Little
    } else {
        ByteOrder::Big
    };
    let body = &data[1..];
    let mut cur = Cursor::new(body);
    let _ = decode_pv_field(intro(), &mut cur, order);
});
