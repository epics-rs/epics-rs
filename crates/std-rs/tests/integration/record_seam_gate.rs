//! std-rs's half of the record-processing runtime-seam census.
//!
//! The rule and its message live in `record-seam-gate`; this file supplies the
//! roots and the exemption, which is the part that is this crate's own. See
//! that crate's module docs for why record support may not name the ambient
//! seam.
//!
//! Roots, not a list: the hand-written list named six files while ten sit
//! under the two roots, so a new record or device support was outside the
//! census until someone remembered to add it. The walk covers it on creation.

/// No source under the roots may name the ambient seam.
const EXEMPT: &[(&str, &str)] = &[];

#[test]
fn a_record_support_tail_never_uses_the_ambient_seam() {
    record_seam_gate::assert_no_ambient_seam_in_tree(
        env!("CARGO_MANIFEST_DIR"),
        &["src/records", "src/device_support"],
        EXEMPT,
    );
}
