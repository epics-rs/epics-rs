//! std-rs's half of the record-processing runtime-seam census.
//!
//! The rule and its message live in `record-seam-gate`; this file supplies the
//! list, which is the part that is this crate's own. See that crate's module
//! docs for why record support may not name the ambient seam.

/// Every production source the framework can reach from a record-processing
/// callback. `device_support/epid_fast.rs` is deliberately absent: its three
/// `tokio::spawn`s start the fast-EPID interrupt loop, a long-lived task the
/// caller starts from its own runtime through the public
/// `start_callback_loop*` entries — never from a `Record` or `DeviceSupport`
/// method. Moving that loop to the background executor would pin a
/// callback-pool worker for the life of the IOC, which is the opposite of the
/// fix.
const SOURCES: &[(&str, &str)] = &[
    (
        "records/throttle.rs",
        include_str!("../src/records/throttle.rs"),
    ),
    ("records/epid.rs", include_str!("../src/records/epid.rs")),
    (
        "records/timestamp.rs",
        include_str!("../src/records/timestamp.rs"),
    ),
    (
        "device_support/epid_soft.rs",
        include_str!("../src/device_support/epid_soft.rs"),
    ),
    (
        "device_support/epid_soft_callback.rs",
        include_str!("../src/device_support/epid_soft_callback.rs"),
    ),
    (
        "device_support/time_of_day.rs",
        include_str!("../src/device_support/time_of_day.rs"),
    ),
];

#[test]
fn a_record_support_tail_never_uses_the_ambient_seam() {
    record_seam_gate::assert_no_ambient_seam(SOURCES);
}
