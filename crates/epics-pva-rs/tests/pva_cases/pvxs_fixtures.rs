//! pvxs-binary-derived golden bytes — sole source of truth.
//!
//! Bytes here came out of pvxs's own `to_wire` at run time via
//! `tools/pvxs-golden-capture/capture.cpp` (linked against
//! `libpvxs.a`). The fixture file is committed verbatim; tests
//! look up the relevant key with `golden(key)` instead of hard-
//! coding hex inline. A goldens-derived-from-source approach can
//! share blind spots with the encoder under test — captured
//! bytes can't.
//!
//! To re-capture (e.g. after a pvxs upgrade):
//!
//! ```text
//! crates/epics-pva-rs/tools/pvxs-golden-capture/build.sh
//! crates/epics-pva-rs/tools/pvxs-golden-capture/capture \
//!     > crates/epics-pva-rs/tools/pvxs-golden-capture/fixtures.txt
//! cargo nextest run -p epics-pva-rs --profile interop
//! ```
//!
//! A diff in `fixtures.txt` is the wire-shape change report.

const FIXTURES: &str = include_str!("../../tools/pvxs-golden-capture/fixtures.txt");

/// Return the pvxs-captured hex string for `key`. Panics if absent
/// so a typo surfaces as a clear test failure, not a silent
/// always-pass.
pub fn golden(key: &str) -> &'static str {
    for line in FIXTURES.lines() {
        if let Some((k, v)) = line.split_once('=')
            && k == key
        {
            return v;
        }
    }
    panic!(
        "pvxs golden fixture missing: {key} \
         (re-run tools/pvxs-golden-capture/capture or add the fixture)"
    )
}
