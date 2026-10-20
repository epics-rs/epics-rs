//! `stdlib::{strtol, strtoul}` past `u128`: glibc keeps saturating however
//! many digits follow, so a magnitude that wraps the accumulator back into
//! range must still read as `ERANGE`. Expected values are glibc 2.39 under
//! `-D_GNU_SOURCE`.

use std::ffi::{c_long, c_ulong};

use epics_libcom_rs::runtime::stdlib::{strtol, strtoul};

/// `2^128 + 5`: the 33rd hex digit takes the accumulator past `u128`, where a
/// wrapping multiply would leave just the `5`.
const HEX_2_128_PLUS_5: &str = "0x100000000000000000000000000000005";

#[test]
fn magnitude_past_u128_saturates() {
    assert_eq!(strtol(HEX_2_128_PLUS_5, 0), (c_long::MAX, 35));
    assert_eq!(strtol(format!("-{HEX_2_128_PLUS_5}"), 0), (c_long::MIN, 36));
    assert_eq!(strtoul(HEX_2_128_PLUS_5, 0), (c_ulong::MAX, 35));
    let dec = "1000000000000000000000000000000000000000005";
    assert_eq!(strtol(dec, 10), (c_long::MAX, 43));
    assert_eq!(strtoul(dec, 10), (c_ulong::MAX, 43));
}
