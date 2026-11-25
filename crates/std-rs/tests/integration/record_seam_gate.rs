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

/// The one source under the roots that may name the ambient seam.
///
/// `device_support/epid_fast.rs`'s three `tokio::spawn`s start the fast-EPID
/// interrupt loop, a long-lived task the caller starts from its own runtime
/// through the public `start_callback_loop*` entries — never from a `Record`
/// or `DeviceSupport` method. Moving that loop to the background executor
/// would pin a callback-pool worker for the life of the IOC, which is the
/// opposite of the fix.
const EXEMPT: &[(&str, &str)] = &[(
    "src/device_support/epid_fast.rs",
    "long-lived fast-EPID interrupt loop, started by the caller from its own \
     runtime via start_callback_loop*, never from a record callback",
)];

#[test]
fn a_record_support_tail_never_uses_the_ambient_seam() {
    record_seam_gate::assert_no_ambient_seam_in_tree(
        env!("CARGO_MANIFEST_DIR"),
        &["src/records", "src/device_support"],
        EXEMPT,
    );
}
