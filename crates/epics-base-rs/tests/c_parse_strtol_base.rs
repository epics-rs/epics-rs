//! `c_parse::{strtol, strtoul}` at the edges of the base range. glibc refuses
//! base 1 and anything above 36 with `EINVAL` before reading a byte: 0 back
//! and nothing consumed. Bases 0, 2 and 36 still scan. Expected values are
//! glibc 2.39 under `-D_GNU_SOURCE`.

use epics_base_rs::types::c_parse::{strtol, strtoul};

const INPUTS: [&str; 5] = ["  -12x", "zz", "0b101", "0x1f", "1"];

#[test]
fn base_1_is_refused() {
    for s in INPUTS {
        assert_eq!(strtol(s, 1), (0, 0), "{s:?}");
        assert_eq!(strtoul(s, 1), (0, 0), "{s:?}");
    }
}

#[test]
fn base_above_36_is_refused() {
    for base in [37, 64, u32::MAX] {
        for s in INPUTS {
            assert_eq!(strtol(s, base), (0, 0), "{s:?} base {base}");
            assert_eq!(strtoul(s, base), (0, 0), "{s:?} base {base}");
        }
    }
}

/// `(base, input, strtol, strtoul, end)`.
const SCANS: [(u32, &str, i64, u64, usize); 15] = [
    (0, "  -12x", -12, 18446744073709551604, 5),
    (0, "zz", 0, 0, 0),
    (0, "0b101", 5, 5, 5),
    (0, "0x1f", 31, 31, 4),
    (0, "1", 1, 1, 1),
    (2, "  -12x", -1, u64::MAX, 4),
    (2, "zz", 0, 0, 0),
    (2, "0b101", 5, 5, 5),
    (2, "0x1f", 0, 0, 1),
    (2, "1", 1, 1, 1),
    (36, "  -12x", -1401, 18446744073709550215, 6),
    (36, "zz", 1295, 1295, 2),
    (36, "0b101", 514513, 514513, 5),
    (36, "0x1f", 42819, 42819, 4),
    (36, "1", 1, 1, 1),
];

#[test]
fn bases_0_2_36_still_scan() {
    for (base, s, l, u, end) in SCANS {
        assert_eq!(strtol(s, base), (l, end), "{s:?} base {base}");
        assert_eq!(strtoul(s, base), (u, end), "{s:?} base {base}");
    }
}
