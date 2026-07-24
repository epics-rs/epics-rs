use std::time::{Duration, Instant, SystemTime};

use crate::walltime::WallTime;

/// Current wall-clock time as a [`WallTime`].
///
/// Returns [`WallTime`] rather than [`SystemTime`] so a snapshot built from
/// "now" shares one timestamp type with snapshots built from exact wire
/// integers. The OS clock is still read via [`SystemTime::now`]; on Windows
/// that clock is itself 100 ns-granular, which `WallTime` does not change.
pub fn now_wall() -> WallTime {
    SystemTime::now().into()
}

pub fn now_mono() -> Instant {
    Instant::now()
}

pub fn deadline_from_now(d: Duration) -> Instant {
    Instant::now() + d
}

/// The OS clock-tick period, in seconds — C `epicsThreadSleepQuantum()`.
///
/// posix (`libcom/src/osi/os/posix/osdThread.c:1108-1116`):
///
/// ```c
/// double epicsThreadSleepQuantum(void)
/// {
///     double hz = sysconf(_SC_CLK_TCK);
///     if (hz <= 0) return 0.0;
///     return 1.0 / hz;
/// }
/// ```
///
/// Records use it to round a delay field to a whole number of ticks — e.g.
/// `sseqRecord.c:197-200` quantizes every `DLYn` at init. Returns 0.0 when the
/// tick rate is unavailable, exactly as C does; callers must treat that as "no
/// quantization" rather than dividing by it.
///
/// # Why this asks instead of stating
///
/// The tick rate is not ours to declare. On RTEMS it is set by
/// `CONFIGURE_MICROSECONDS_PER_TICK` in `epics-rtems-boot`'s
/// `csrc/rtems_config.c`, which is deliberately `#ifndef`-overridable from the
/// build so a timing experiment needs no source edit. A Rust constant here
/// would be a second copy of that number, silently wrong the first time
/// anyone overrides it — with nothing checking the two agree.
///
/// So the unix arm asks, and the answer comes from the same define:
///
/// ```text
/// sysconf(_SC_CLK_TCK)
///   -> rtems_clock_get_ticks_per_second()      cpukit/posix/src/sysconf.c:60-61
///   -> _Watchdog_Ticks_per_second              rtems/rtems/clock.h:871
///    = 1000000 / CONFIGURE_MICROSECONDS_PER_TICK   confdefs/clock.h:100-101
/// ```
///
/// This is also what C itself does on RTEMS — `RTEMS-score/osdThread.c:860-865`
/// returns `1.0 / rtemsTicksPerSecond_double` rather than a constant — so the
/// port matches C's behaviour on the target, not just on posix.
///
/// The non-unix (Windows) arm keeps a constant. `_SC_CLK_TCK` is 100 on Linux
/// and macOS, and 100 Hz is the historical default this port shipped; it
/// restates no `#define` of ours. It is *not* C parity: `WIN32/osdThread.c:906-932`
/// asks `GetSystemTimeAdjustment` and returns 0.0 on failure. Closing that gap
/// needs a Windows syscall dependency this crate does not have, and is a
/// separate change from the RTEMS one.
pub fn thread_sleep_quantum() -> f64 {
    #[cfg(unix)]
    {
        // SAFETY: `sysconf` is a pure query with no preconditions.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        if hz <= 0.0 { 0.0 } else { 1.0 / hz }
    }
    #[cfg(not(unix))]
    {
        0.01
    }
}

/// Round `seconds` to the nearest whole [`thread_sleep_quantum`] tick, the way
/// C records do it:
///
/// ```c
/// #define NINT(f) (long)((f)>0 ? (f)+0.5 : (f)-0.5)
/// plinkGroup->dly = epicsThreadSleepQuantum() *
///                   NINT(plinkGroup->dly / epicsThreadSleepQuantum());
/// ```
///
/// (`sseqRecord.c:67`, `:197-199`.) The `NINT` cast is to a C `long` (i64),
/// NOT an f64 round, and the served DLY must reproduce that cast byte-for-byte
/// — see `c_long_cast`. Two boundaries C's cast owns that an `f64::trunc`
/// port gets wrong:
///
///   * **Overflow.** A `dly` large enough that `ticks` rounds past 2^63
///     overflows the `(long)` cast. On x86-64 (the target the oracle runs on)
///     `cvttsd2si` maps every out-of-range value — and NaN/±inf — to
///     i64::MIN = `0x8000_0000_0000_0000`, so the field becomes `quantum *
///     i64::MIN` ≈ -9.22e16, exactly what C serves for a huge `caput`. An
///     `f64::trunc` port instead keeps the huge value (or `inf`).
///   * **Negative zero.** A `dly` that rounds to zero yields the *integer* 0,
///     and `quantum * 0` is `+0.0`. An `f64::trunc` port produces `-0.0` for
///     the `0.0` default (`(-0.0 - 0.5).trunc()` = `-0.0`) and for any tiny or
///     negative input that rounds to zero, which renders as `-0` where C
///     renders `0`.
///
/// With a zero quantum (C's `hz <= 0` path) the value is returned unchanged
/// rather than dividing by zero.
pub fn quantize_to_sleep_quantum(seconds: f64) -> f64 {
    let quantum = thread_sleep_quantum();
    if quantum <= 0.0 {
        return seconds;
    }
    let ticks = seconds / quantum;
    // C `NINT(f) = (long)((f) > 0 ? (f) + 0.5 : (f) - 0.5)`.
    let rounded = if ticks > 0.0 {
        ticks + 0.5
    } else {
        ticks - 0.5
    };
    quantum * c_long_cast(rounded) as f64
}

/// Reproduce C's `(long)` cast of a `double` with x86-64 `cvttsd2si`
/// semantics: an in-range finite value truncates toward zero (as Rust's
/// `as i64` already does); every out-of-range value and NaN/±inf yields
/// i64::MIN, the "integer indefinite" the instruction returns.
///
/// Rust's own `as i64` *saturates* out-of-range inputs instead (2^63 →
/// i64::MAX, -inf → i64::MIN, NaN → 0), so the explicit range check is what
/// makes the port match C on the overflow boundary.
fn c_long_cast(f: f64) -> i64 {
    // i64::MIN is exactly -2^63 and representable as f64; i64::MAX rounds up to
    // 2^63 as f64 (out of range), so the upper bound is a strict `< 2^63`.
    const MIN: f64 = -9_223_372_036_854_775_808.0; // -2^63
    const LIMIT: f64 = 9_223_372_036_854_775_808.0; //  2^63
    if f.is_nan() || f < MIN || f >= LIMIT {
        i64::MIN
    } else {
        f as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything before the first column-0 `#[cfg(test)]` — the code that
    /// actually ships. Same helper as `epics-pva-rs`'s source guards.
    fn production_scope(src: &str) -> &str {
        match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        }
    }

    /// The tick rate must be ASKED for, never restated.
    ///
    /// `CONFIGURE_MICROSECONDS_PER_TICK` in `epics-rtems-boot`'s
    /// `csrc/rtems_config.c` owns the number, and it is `#ifndef`-overridable
    /// from the build. A Rust constant restating it is a second source of
    /// truth that goes silently wrong the first time anyone overrides it.
    ///
    /// Fails today, on Linux, with no cross toolchain.
    #[test]
    fn the_tick_rate_is_asked_for_not_restated() {
        // Comment lines are stripped: the doc above `thread_sleep_quantum`
        // spells out `1000000 / CONFIGURE_MICROSECONDS_PER_TICK` to explain
        // the chain, and that text must not read as a restatement.
        let src: String = production_scope(include_str!("time.rs"))
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let src = src.as_str();

        assert!(
            src.contains("libc::sysconf(libc::_SC_CLK_TCK)"),
            "thread_sleep_quantum must ask the OS for the tick rate"
        );

        // The defect this replaced: asking only on Linux, and handing every
        // other unix — RTEMS included — a constant.
        assert!(
            !src.contains("#[cfg(target_os = \"linux\")]"),
            "the tick-rate arm must select on `unix`, not on `linux`: RTEMS is \
             a unix that answers _SC_CLK_TCK from the boot crate's define"
        );
        assert_eq!(
            src.matches("#[cfg(unix)]").count(),
            1,
            "exactly one arm asks; if a second appears, this guard needs updating"
        );

        // 10 ms expressed any of the ways someone would naturally write it.
        // The sole surviving literal is the documented Windows arm.
        assert_eq!(
            src.matches("0.01").count(),
            1,
            "0.01 may appear only once, in the non-unix arm"
        );
        for restatement in ["10000", "10_000", "0.010", "1e-2"] {
            assert!(
                !src.contains(restatement),
                "`{restatement}` restates CONFIGURE_MICROSECONDS_PER_TICK; \
                 read it back through sysconf instead"
            );
        }
    }

    #[test]
    fn test_now_wall() {
        let t = now_wall();
        assert!(t.since_unix_epoch().as_secs() > 0);
    }

    #[test]
    fn test_now_mono() {
        let t1 = now_mono();
        let t2 = now_mono();
        assert!(t2 >= t1);
    }

    #[test]
    fn test_deadline_from_now() {
        let before = Instant::now();
        let deadline = deadline_from_now(Duration::from_secs(10));
        assert!(deadline > before);
        assert!(deadline <= before + Duration::from_secs(11));
    }

    /// C's `(long)` cast, boundary by boundary: in-range truncation toward
    /// zero, and i64::MIN for everything x86-64 `cvttsd2si` cannot represent.
    #[test]
    fn c_long_cast_matches_cvttsd2si() {
        // In range: plain truncation toward zero, both signs.
        assert_eq!(c_long_cast(0.0), 0);
        assert_eq!(c_long_cast(-0.0), 0);
        assert_eq!(c_long_cast(0.5), 0);
        assert_eq!(c_long_cast(-0.5), 0); // toward zero, NOT -1
        assert_eq!(c_long_cast(1.9), 1);
        assert_eq!(c_long_cast(-1.9), -1);
        // The representable extremes.
        assert_eq!(c_long_cast(-9_223_372_036_854_775_808.0), i64::MIN);
        // Out of range and non-finite all collapse to the "integer indefinite".
        assert_eq!(c_long_cast(9_223_372_036_854_775_808.0), i64::MIN); // +2^63
        assert_eq!(c_long_cast(1e300), i64::MIN);
        assert_eq!(c_long_cast(-1e300), i64::MIN);
        assert_eq!(c_long_cast(f64::INFINITY), i64::MIN);
        assert_eq!(c_long_cast(f64::NEG_INFINITY), i64::MIN);
        assert_eq!(c_long_cast(f64::NAN), i64::MIN);
    }

    /// The DLY quantization the served value must match, by invariant boundary.
    #[test]
    fn quantize_dly_boundaries_match_c() {
        let q = thread_sleep_quantum();
        assert!(q > 0.0, "test assumes a positive clock quantum, got {q}");

        // The default DLY is 0.0; it must serve as +0.0, never -0.0.
        let zero = quantize_to_sleep_quantum(0.0);
        assert_eq!(zero, 0.0);
        assert!(
            zero.is_sign_positive(),
            "DLY=0.0 must round to +0.0 (renders \"0\"), got a negative zero"
        );

        // A -0.0 input must also normalize to +0.0 (C's `NINT` yields integer 0).
        let neg_zero = quantize_to_sleep_quantum(-0.0);
        assert_eq!(neg_zero, 0.0);
        assert!(
            neg_zero.is_sign_positive(),
            "DLY=-0.0 must round to +0.0, got a negative zero"
        );

        // A tiny positive below half a tick rounds to +0.0.
        let tiny = quantize_to_sleep_quantum(q / 4.0);
        assert_eq!(tiny, 0.0);
        assert!(tiny.is_sign_positive(), "tiny +dly rounds to +0.0");

        // A small negative that rounds to zero: C truncates `-0.x` to 0 → +0.0.
        let tiny_neg = quantize_to_sleep_quantum(-q / 4.0);
        assert_eq!(tiny_neg, 0.0);
        assert!(
            tiny_neg.is_sign_positive(),
            "a -dly rounding to zero must serve +0.0, not -0.0"
        );

        // An exact quantum multiple is preserved exactly.
        assert_eq!(quantize_to_sleep_quantum(3.0 * q), 3.0 * q);
        assert_eq!(quantize_to_sleep_quantum(-3.0 * q), -3.0 * q);

        // Round-half-away-from-zero at the tick boundary.
        assert_eq!(quantize_to_sleep_quantum(1.5 * q), 2.0 * q);
        assert_eq!(quantize_to_sleep_quantum(-1.5 * q), -2.0 * q);

        // A huge finite `dly` overflows the `(long)` cast to i64::MIN, so the
        // served value is `quantum * i64::MIN` — negative, and NOT `inf`.
        let huge = quantize_to_sleep_quantum(1e300);
        assert_eq!(huge, q * (i64::MIN as f64));
        assert!(
            huge.is_finite() && huge < 0.0,
            "a huge +dly must serve C's ~-9.22e16, not inf, got {huge}"
        );
        // A huge negative `dly` overflows the same way.
        let huge_neg = quantize_to_sleep_quantum(-1e300);
        assert_eq!(huge_neg, q * (i64::MIN as f64));

        // ±inf also collapses to i64::MIN (an `f64::trunc` port kept inf).
        assert_eq!(
            quantize_to_sleep_quantum(f64::INFINITY),
            q * (i64::MIN as f64)
        );
        assert_eq!(
            quantize_to_sleep_quantum(f64::NEG_INFINITY),
            q * (i64::MIN as f64)
        );
    }
}
