//! Default-suite decoder coverage: feed every captured forward-
//! golden through the Rust decoder and assert it reproduces the
//! original `(FieldDesc, PvField)` semantically (and the value
//! re-encoded from the decoded form is byte-identical to the
//! input — proves no information loss).
//!
//! Why this is a real assertion: the forward goldens are bytes
//! pvxs's pvxget accepted. If the Rust decoder reproduces those
//! values bit-for-bit on the way back, then Rust's decoder is
//! bytes-compatible with pvxs's encoder for every shape in the
//! matrix — without needing pvxs at runtime.
//!
//! Trust chain:
//!   forward interop: pvxs accepts Rust bytes
//!     → bytes frozen (forward golden test)
//!     → decoder replay: Rust decoder reconstructs the same value
//!       from those same bytes
//!
//! No external dep — runs on any host.

#[path = "interop_helpers/pv_builders.rs"]
mod pv_builders;

use epics_pva_rs::proto::ByteOrder;
use epics_pva_rs::pvdata::{PvField, ScalarValue, encode::*};
use pv_builders::{complex_pv_matrix, split_fixture};

use std::io::Cursor;
use std::path::PathBuf;

fn fixture_path(pv: &str) -> PathBuf {
    let stem = pv.replace([':', '/'], "_");
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pvxs")
        .join(format!("{stem}.bin"))
}

/// Normalise `ScalarArrayTyped` (decoder fast path) into the
/// untyped boxed form so equality holds against the
/// `complex_pv_matrix` originals.
fn normalise_array_variants(v: &mut PvField) {
    use epics_pva_rs::pvdata::PvStructure;
    match v {
        PvField::ScalarArrayTyped(t) => {
            *v = PvField::ScalarArray(t.to_scalar_values());
        }
        PvField::Structure(PvStructure { fields, .. }) => {
            for (_, child) in fields.iter_mut() {
                normalise_array_variants(child);
            }
        }
        PvField::StructureArray(items) => {
            // present elements only; `None` is a null element.
            for child in items.iter_mut().flatten() {
                for (_, leaf) in child.fields.iter_mut() {
                    normalise_array_variants(leaf);
                }
            }
        }
        PvField::Union { value, .. } => {
            normalise_array_variants(value);
        }
        PvField::Variant(vv) => {
            normalise_array_variants(&mut vv.value);
        }
        _ => {}
    }
}

#[test]
fn wire_golden_decode_roundtrip() {
    let mut failures: Vec<String> = Vec::new();

    for mut build in complex_pv_matrix() {
        let path = fixture_path(build.name);
        let Ok(golden) = std::fs::read(&path) else {
            failures.push(format!("{} → fixture missing at {:?}", build.name, path));
            continue;
        };
        let Some((desc_bytes, value_bytes)) = split_fixture(&golden) else {
            failures.push(format!("{} → fixture failed to split", build.name));
            continue;
        };

        // Decode descriptor (Rust decoder, fresh TypeStore).
        let mut desc_cur = Cursor::new(desc_bytes);
        let desc_decoded = match decode_type_desc(&mut desc_cur, ByteOrder::Little) {
            Ok(d) => d,
            Err(e) => {
                failures.push(format!("{}: descriptor decode failed: {e:?}", build.name));
                continue;
            }
        };
        if desc_decoded != build.desc {
            failures.push(format!(
                "{}: descriptor mismatch.\n  want: {:?}\n  got:  {:?}",
                build.name, build.desc, desc_decoded,
            ));
            continue;
        }

        // Decode value against the decoded descriptor.
        let mut val_cur = Cursor::new(value_bytes);
        let mut value_decoded =
            match decode_pv_field(&desc_decoded, &mut val_cur, ByteOrder::Little) {
                Ok(v) => v,
                Err(e) => {
                    failures.push(format!("{}: value decode failed: {e:?}", build.name));
                    continue;
                }
            };
        normalise_array_variants(&mut value_decoded);
        // Normalise the expected side too. The comparison is about VALUES, not
        // which array representation a builder happened to pick: NTNDArray now
        // builds `ScalarArrayTyped` (an `Arc<[T]>` the encoder bulk-copies)
        // where it used to build `ScalarArray`, and both encode to identical
        // wire bytes -- which the re-encode check below is what actually
        // proves.
        normalise_array_variants(&mut build.value);
        if value_decoded != build.value {
            failures.push(format!(
                "{}: value mismatch.\n  want: {:?}\n  got:  {:?}",
                build.name, build.value, value_decoded,
            ));
            continue;
        }

        // Round-trip: re-encode the decoded value, assert the new
        // bytes match the fixture. Proves the decoder→encoder cycle
        // is lossless for this shape.
        let mut re_value = Vec::new();
        encode_pv_field(
            &value_decoded,
            &desc_decoded,
            ByteOrder::Little,
            &mut re_value,
        );
        if re_value != value_bytes {
            failures.push(format!(
                "{}: re-encoded value bytes differ from fixture.\n  fixture: {}\n  re-enc:  {}",
                build.name,
                hex(value_bytes),
                hex(&re_value),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} decode-roundtrip failure(s):\n{}",
        failures.len(),
        failures.join("\n----\n"),
    );

    // Touch ScalarValue (used by complex_pv_matrix indirectly) so
    // the unused-import lint stays quiet if the matrix ever drops
    // a leaf type.
    let _ = ScalarValue::Boolean(true);
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        use std::fmt::Write;
        let _ = write!(&mut s, "{byte:02x}");
    }
    s
}
