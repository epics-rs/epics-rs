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

/// The instant `base + d`, saturating where `Instant + Duration` would
/// panic — the single owner of "turn a delay into a deadline".
///
/// [`duration_from_secs`] deliberately maps `+inf`, `NaN` and any
/// magnitude past `Duration`'s range to [`Duration::MAX`], so a deadline
/// computed from a record delay field can be exactly that sum and
/// `Instant`'s `Add` panics on it. tokio's `sleep()` does not: it takes
/// `Instant::now().checked_add(dur)` and falls back to
/// `Instant::far_future()`, about thirty years out. The hosted build
/// therefore slept forever where the exec build unwound the task, and it
/// is that disagreement — not the arithmetic — that is the defect, so
/// the fallback here is tokio's, to the same constant.
///
/// Measured on `armv7-rtems-eabihf` (QEMU `xilinx-zynq-a9`,
/// `realtime-ca-ioc`), which is where the panic was filed: `caput
/// RTEMS:BO.HIGH 1e300` then `caput RTEMS:BO 1` makes `bo::process` arm
/// `DelayedCallbackAfter(Duration::MAX)`. With this fallback the guest
/// takes it silently and the one-shot simply never fires — the record
/// still reads `On` eight seconds later, and a 0.5 s one-shot armed
/// afterwards still reverts it. Rebuilding the same image with `base + d`
/// in place of the `checked_add` gives *"panic on thread `cbMedium` at
/// runtime/time.rs: overflow when adding duration to instant"*, so the
/// embedded timer path really does consume this sum and the guard is what
/// keeps it off the console. VxWorks is still arithmetic-only.
pub fn deadline_after(base: Instant, d: Duration) -> Instant {
    base.checked_add(d).unwrap_or_else(far_future)
}

/// `base` = now. See [`deadline_after`].
pub fn deadline_from_now(d: Duration) -> Instant {
    deadline_after(Instant::now(), d)
}

/// Roughly thirty years — the offset in tokio's `Instant::far_future()`,
/// the "never fires" deadline an unrepresentable one collapses to.
/// Shared with [`crate::runtime::task::deadline_after`], which applies
/// the same rule to the runtime's own `Instant` alias.
pub(crate) const FAR_FUTURE: Duration = Duration::from_secs(86_400 * 365 * 30);

/// See [`FAR_FUTURE`].
fn far_future() -> Instant {
    Instant::now() + FAR_FUTURE
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
/// `sseqRecord.c:198-200` quantizes every `DLYn` at init. Returns 0.0 when the
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
///   -> rtems_clock_get_ticks_per_second()      cpukit/posix/src/sysconf.c:60-61 (rtems_6)
///   -> _Watchdog_Ticks_per_second              rtems/rtems/clock.h:871 (both rtems pins)
///    = 1000000 / CONFIGURE_MICROSECONDS_PER_TICK   confdefs/clock.h:100-101 (rtems_6)
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
/// (`sseqRecord.c:67`, `:198-199`.) The `NINT` cast is to a C `long` (i64),
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

/// Seconds as an `f64` → [`Duration`], without the panic
/// `Duration::from_secs_f64` raises.
///
/// This is the libcom time seam's single converter, and every caller
/// that turns a *record field*, an *environment variable* or any other
/// externally supplied `double` into a delay must come through it.
/// `Duration::from_secs_f64` panics on NaN, on either infinity, on a
/// negative, and on a finite value past `u64::MAX` seconds — and an
/// `is_finite()` test at the call site is not the rule, because `1e300`
/// is finite and still panics. `Duration::try_from_secs_f64` is the one
/// rule that covers all four in a single test.
///
/// C never aborts on any of them: `epicsTimeAddSeconds`
/// (`epicsTime.cpp`) does `nsec += epicsInt64(seconds*1e9 + ...)`, an
/// out-of-range float→integer conversion, so the deadline is garbage and
/// the callback fires at the wrong time while the IOC keeps serving
/// every other PV. The mapping here keeps that "IOC survives" property
/// and gives the garbage a defined shape:
///
/// * negative, including `-inf` → [`Duration::ZERO`] — C's
///   already-expired deadline, which fires at once.
/// * `+inf`, `NaN`, or a magnitude beyond `Duration` → [`Duration::MAX`]
///   — a deadline no comparison ever reaches, i.e. it never fires.
///   NaN lands here because in C every `now < expire` test against NaN
///   is false, which is the same "never fires".
pub fn duration_from_secs(secs: f64) -> Duration {
    Duration::try_from_secs_f64(secs).unwrap_or(if secs < 0.0 {
        Duration::ZERO
    } else {
        Duration::MAX
    })
}

/// Block the calling thread for `secs` — C `epicsThreadSleep`, and the
/// single owner of "turn a caller-supplied delay into a sleep".
///
/// C (`libcom/src/osi/os/posix/osdThread.c:916-934` @R7.0.10) truncates
/// `seconds` into `timespec.tv_sec`, zeroes the delay when
/// `seconds <= 0`, and lets `nanosleep` reject whatever will not fit.
/// Measured against `bin/linux-x86_64/softIoc` driving
/// `epicsThreadSleep`, `1e300`, `inf`, `nan` and `-5` each return inside
/// the process's own 0.33 s startup while `0.25` and `1.5` sleep their
/// full delay: a delay that is not a representable positive `Duration`
/// is not slept at all.
///
/// Deliberately NOT [`duration_from_secs`]. That owner saturates to
/// [`Duration::MAX`] because a *deadline* built from an absurd delay
/// must never fire; sleeping on [`Duration::MAX`] would park the caller
/// for 584 billion years exactly where C returns at once, and
/// `Duration::from_secs_f64` would panic. The two meanings need two
/// owners, not one conversion reused on both sides.
pub fn sleep_secs(secs: f64) {
    if let Ok(d) = Duration::try_from_secs_f64(secs) {
        std::thread::sleep(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use source_guard::{Comments, production};

    /// The sleep owner's boundaries are the *opposite* of the deadline
    /// owner's, which is the whole reason it exists: everything
    /// `duration_from_secs` saturates to [`Duration::MAX`] is a delay
    /// `nanosleep` refuses, so C returns from it at once and so must we.
    ///
    /// The pairs measured against `softIoc`: `1e300`, `inf`, `nan`, `-5`
    /// returned in C's 0.33 s startup baseline; `0.25` and `1.5` slept.
    #[test]
    fn sleep_secs_returns_at_once_on_every_delay_nanosleep_refuses() {
        for refused in [
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            1e300,
            -5.0,
            u64::MAX as f64,
        ] {
            let t = Instant::now();
            sleep_secs(refused);
            assert!(
                t.elapsed() < Duration::from_millis(50),
                "sleep_secs({refused}) must return at once, as C's nanosleep does"
            );
        }
        // Zero is representable, and C's `nanosleep(0,0)` also returns
        // at once — it must not be confused with the refused set.
        let t = Instant::now();
        sleep_secs(0.0);
        assert!(t.elapsed() < Duration::from_millis(50));
        // A representable positive delay is slept in full.
        let t = Instant::now();
        sleep_secs(0.05);
        assert!(t.elapsed() >= Duration::from_millis(50));
    }

    /// Boundaries of the one rule, not scenarios: every input
    /// `Duration::from_secs_f64` would panic on has a defined answer
    /// here, and the representable ones convert unchanged.
    #[test]
    fn duration_from_secs_covers_every_panic_boundary() {
        assert_eq!(duration_from_secs(f64::INFINITY), Duration::MAX);
        assert_eq!(duration_from_secs(f64::NEG_INFINITY), Duration::ZERO);
        assert_eq!(duration_from_secs(f64::NAN), Duration::MAX);
        // Finite and far too large — the case an `is_finite()` guard
        // lets through and `from_secs_f64` still panics on.
        assert_eq!(duration_from_secs(1e300), Duration::MAX);
        // The representable edge: `u64::MAX` seconds is out of range,
        // one below the power of two above it is not.
        assert_eq!(duration_from_secs(u64::MAX as f64), Duration::MAX);
        assert_eq!(
            duration_from_secs(9.0e18),
            Duration::try_from_secs_f64(9.0e18).unwrap()
        );
        assert_eq!(duration_from_secs(-1.0), Duration::ZERO);
        assert_eq!(duration_from_secs(-0.0), Duration::ZERO);
        assert_eq!(duration_from_secs(0.0), Duration::ZERO);
        assert_eq!(duration_from_secs(0.25), Duration::from_millis(250));
        assert_eq!(duration_from_secs(2.5), Duration::from_millis(2500));
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
        // `Strip`: the doc above `thread_sleep_quantum` spells out
        // `1000000 / CONFIGURE_MICROSECONDS_PER_TICK` to explain the chain,
        // and that text must not read as a restatement.
        let src = production(include_str!("time.rs"), Comments::Strip);

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

    /// Boundaries of the deadline owner: a representable delay converts
    /// unchanged, and the one `Instant + Duration` panics on —
    /// `Duration::MAX`, which is exactly what `duration_from_secs`
    /// returns for `+inf`, `NaN` and `1e300` — saturates instead.
    #[test]
    fn deadline_saturates_where_instant_add_would_panic() {
        let base = Instant::now();
        assert_eq!(
            deadline_after(base, Duration::from_secs(10)),
            base + Duration::from_secs(10)
        );
        let never = deadline_after(base, Duration::MAX);
        assert!(never > base + Duration::from_secs(86_400 * 365));
        assert!(deadline_from_now(duration_from_secs(f64::INFINITY)) > Instant::now());
        assert!(deadline_from_now(duration_from_secs(1e300)) > Instant::now());
        // Saturating twice still saturates.
        assert!(deadline_after(never, Duration::MAX) > base);
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
