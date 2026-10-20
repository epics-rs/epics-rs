//! `c_parse::{strtol, strtoul}` past `u128`: glibc keeps saturating however
//! many digits follow, so a magnitude that wraps the accumulator back into
//! range must still read as `ERANGE`. Expected values are glibc 2.39 under
//! `-D_GNU_SOURCE`.

use epics_base_rs::types::c_parse::{strtol, strtoul};

/// `2^128 + 5`: the 33rd hex digit takes the accumulator past `u128`, where a
/// wrapping multiply would leave just the `5`.
const HEX_2_128_PLUS_5: &str = "0x100000000000000000000000000000005";

#[test]
fn magnitude_past_u128_saturates() {
    assert_eq!(strtol(HEX_2_128_PLUS_5, 0), (i64::MAX, 35));
    assert_eq!(strtol(format!("-{HEX_2_128_PLUS_5}"), 0), (i64::MIN, 36));
    assert_eq!(strtoul(HEX_2_128_PLUS_5, 0), (u64::MAX, 35));
    let dec = "1000000000000000000000000000000000000000005";
    assert_eq!(strtol(dec, 10), (i64::MAX, 43));
    assert_eq!(strtoul(dec, 10), (u64::MAX, 43));
}
