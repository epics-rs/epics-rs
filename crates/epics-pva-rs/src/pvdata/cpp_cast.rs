//! C++ scalar-cast semantics for the double → integer step, so a leaf the
//! port writes carries the same bytes pvxs would put there.
//!
//! `Value::copyIn` (`pvxs/src/data.cpp:535-578`) assigns a double into an
//! integer store with a C-style cast — `dest = Dest(*reinterpret_cast<const
//! double*>(ptr))` at `:521`, `Dest` being `int64_t` for a signed store and
//! `uint64_t` for an unsigned one — and only then truncates to the leaf's
//! declared width (`int8_t(orig)` / `uint8_t(orig)`, `:556-573`).
//!
//! Out of range, that cast is undefined behaviour in C++, so "what pvxs does"
//! is really "what the compiler emits on the host". Every EPICS deployment of
//! pvxs this port targets is x86-64/aarch64 with the same shape: `cvttsd2si`
//! (and its aarch64 `fcvtzs` counterpart) returns the *integer indefinite*
//! value `0x8000_0000_0000_0000` for NaN and for anything that does not fit.
//! Rust's `as` instead SATURATES — NaN yields 0 and overflow clamps to
//! `MIN`/`MAX` — which is why the port and C disagreed on exactly the leaves
//! whose value is out of range.
//!
//! Both branches are pinned against values measured from a real `softIocPVX`,
//! not derived from the C++ standard:
//!
//! ```text
//! valueAlarm.*Limit uint64_t  (recGblGetAlarmDouble -> NaN)     C = 9223372036854775808
//! control.limitHigh uint64_t  (getMaxRangeValues DBF_UINT64)    C = 0
//! ```
//!
//! The first is `cvttsd2si(NaN)` reinterpreted unsigned = 2^63. The second is
//! the unsigned path below: 18446744073709551615.0 is not representable as a
//! double and rounds to exactly 2^64, which takes the `>= 2^63` branch, and
//! `cvttsd2si(2^64 - 2^63)` overflows to `0x8000…0`, whose bit 63 the final
//! flip clears — landing on 0.

/// 2^63 — the exact double at which the unsigned conversion changes branch,
/// and the magnitude `cvttsd2si` can no longer represent.
const TWO_POW_63: f64 = 9223372036854775808.0;

/// C++ `int64_t(v)` as x86-64/aarch64 emit it.
///
/// In range, this is Rust's `as` (both truncate toward zero). Out of range or
/// NaN, the hardware yields the integer-indefinite value rather than
/// saturating.
pub(crate) fn double_to_i64(v: f64) -> i64 {
    // `-2^63` is exactly representable and IS in range; `2^63` is not. NaN is
    // unordered, so it is not contained and falls to the indefinite value —
    // `Range::contains` is `start <= v && v < end`, which is the comparison
    // pair this needs.
    if (-TWO_POW_63..TWO_POW_63).contains(&v) {
        v as i64
    } else {
        i64::MIN
    }
}

/// C++ `uint64_t(v)` as x86-64/aarch64 emit it.
///
/// The compiler cannot convert doubles at or above 2^63 with a single
/// `cvttsd2si`, so it emits a branch: below 2^63, convert directly; at or
/// above, subtract 2^63, convert, and flip bit 63 back on. NaN compares
/// unordered — so it takes the *first* branch, and surfaces as
/// `cvttsd2si(NaN)` = 2^63, which is what a real softIocPVX serves for a NaN
/// alarm limit on a `uint64_t` leaf.
pub(crate) fn double_to_u64(v: f64) -> u64 {
    // The branch must be "NOT definitely >= 2^63", not "< 2^63": NaN is
    // unordered and has to take the direct arm, matching the `comisd`/`jnb`
    // pair the compiler emits (unordered sets CF, so the jump is not taken).
    // `partial_cmp` makes the incomparable case explicit — `None` is NaN.
    match v.partial_cmp(&TWO_POW_63) {
        Some(std::cmp::Ordering::Less) | None => double_to_i64(v) as u64,
        _ => (double_to_i64(v - TWO_POW_63) as u64) ^ (1u64 << 63),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two values a real softIocPVX serves, which are the reason this
    /// module exists. Rust's `as` gives 0 for both.
    #[test]
    fn the_measured_softiocpvx_values() {
        // `black_box` only to keep the contrast honest: casting a literal NaN
        // is a compile-time-known value clippy rejects, but the saturation it
        // shows is exactly what this module exists to stop doing.
        let nan = std::hint::black_box(f64::NAN);

        // recGblGetAlarmDouble writes NaN; the leaf is uint64_t.
        assert_eq!(double_to_u64(f64::NAN), 9223372036854775808);
        assert_eq!(nan as u64, 0, "Rust saturates; C does not");

        // getMaxRangeValues(DBF_UINT64) writes 18446744073709551615.0, which
        // is really 2^64 once rounded into a double.
        assert_eq!(double_to_u64(18446744073709551615.0), 0);
        assert_eq!(
            18446744073709551615.0_f64 as u64,
            u64::MAX,
            "Rust saturates; C wraps to 0"
        );
    }

    /// In range, the C++ cast and Rust's `as` agree — the fix must not move
    /// the ordinary case.
    #[test]
    fn in_range_matches_rust_as() {
        for v in [0.0, 1.0, -1.0, 42.5, -42.5, 1e18, -1e18, 32767.0, -32768.0] {
            assert_eq!(double_to_i64(v), v as i64, "i64 {v}");
        }
        for v in [0.0, 1.0, 42.5, 1e18, 65535.0, 4294967295.0] {
            assert_eq!(double_to_u64(v), v as u64, "u64 {v}");
        }
    }

    /// The signed boundary: -2^63 is representable and in range, +2^63 is not.
    #[test]
    fn signed_boundaries() {
        assert_eq!(double_to_i64(-9223372036854775808.0), i64::MIN);
        assert_eq!(
            double_to_i64(9223372036854775808.0),
            i64::MIN,
            "out of range"
        );
        assert_eq!(double_to_i64(1e300), i64::MIN, "recGbl DBF_DOUBLE range");
        assert_eq!(double_to_i64(-1e300), i64::MIN);
        assert_eq!(double_to_i64(f64::NAN), i64::MIN);
        assert_eq!(double_to_i64(f64::INFINITY), i64::MIN);
        assert_eq!(double_to_i64(f64::NEG_INFINITY), i64::MIN);
    }

    /// The unsigned branch boundary at 2^63, and the negative case: C++ wraps
    /// a negative double modularly, Rust's `as` clamps it to 0.
    #[test]
    fn unsigned_boundaries() {
        assert_eq!(double_to_u64(TWO_POW_63), 1u64 << 63, "exactly 2^63");
        assert_eq!(double_to_u64(-1.0), u64::MAX, "wraps, not clamps");
        assert_eq!(-1.0_f64 as u64, 0, "Rust clamps");
        assert_eq!(double_to_u64(1e300), 0, "far out of range");
        assert_eq!(double_to_u64(f64::INFINITY), 0);
    }

    /// The whole table, transcribed from `g++ -O2` on this host compiling the
    /// same two casts pvxs performs. Not derived from the standard (where all
    /// of this is undefined) — read off the compiler that builds pvxs.
    ///
    /// `(input, uint64_t(input), int64_t(input))`
    #[test]
    fn matches_gpp_o2_for_every_probed_value() {
        const PROBE: &[(f64, u64, i64)] = &[
            (f64::NAN, 9223372036854775808, i64::MIN),
            (18446744073709551615.0, 0, i64::MIN),
            (9223372036854775808.0, 9223372036854775808, i64::MIN),
            (-9223372036854775808.0, 9223372036854775808, i64::MIN),
            (1e300, 0, i64::MIN),
            (-1e300, 9223372036854775808, i64::MIN),
            (-1.0, 18446744073709551615, -1),
            (0.0, 0, 0),
            (42.5, 42, 42),
            (65535.0, 65535, 65535),
            (f64::INFINITY, 0, i64::MIN),
        ];
        for &(input, want_u, want_i) in PROBE {
            assert_eq!(double_to_u64(input), want_u, "uint64_t({input})");
            assert_eq!(double_to_i64(input), want_i, "int64_t({input})");
        }
    }

    /// Truncation to the narrower widths is plain modular `as` on the integer
    /// — the same thing `uint8_t(orig)` does at `data.cpp:570`. NaN reaches
    /// them as 2^63 / -2^63, whose low bits are all zero.
    #[test]
    fn narrower_widths_see_zero_for_nan() {
        assert_eq!(double_to_i64(f64::NAN) as i32, 0);
        assert_eq!(double_to_i64(f64::NAN) as i16, 0);
        assert_eq!(double_to_i64(f64::NAN) as i8, 0);
        assert_eq!(double_to_u64(f64::NAN) as u32, 0);
        assert_eq!(double_to_u64(f64::NAN) as u16, 0);
        assert_eq!(double_to_u64(f64::NAN) as u8, 0);
    }
}
