//! `cstr::c_strtol` scans in the base it is given: the timestamp converter's
//! base-10 reads of zero-padded fields (`09`) must not go octal.

use stream_rs::cstr::{CPtr, c_strtol};

#[test]
fn base_10_ignores_leading_zero() {
    assert_eq!(c_strtol(CPtr::new(b"09"), 10), (9, 2));
    assert_eq!(c_strtol(CPtr::new(b"010"), 10), (10, 3));
    assert_eq!(c_strtol(CPtr::new(b"010"), 0), (8, 3));
}
