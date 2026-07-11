use std::time::{Duration, Instant, SystemTime};

use crate::types::WallTime;

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
/// `sseqRecord.c:197-200` quantizes every `DLYn` at init. `_SC_CLK_TCK` is 100
/// on Linux and macOS, hence the 0.01 s fallback on the targets where `libc`
/// is not linked (it is a Linux-only dependency of this crate). Returns 0.0
/// when the tick rate is unavailable, exactly as C does; callers must treat
/// that as "no quantization" rather than dividing by it.
pub fn thread_sleep_quantum() -> f64 {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `sysconf` is a pure query with no preconditions.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        if hz <= 0.0 { 0.0 } else { 1.0 / hz }
    }
    #[cfg(not(target_os = "linux"))]
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
/// (`sseqRecord.c:67`, `:197-199`.) The C truncation-toward-zero cast is
/// reproduced with `f64::trunc`, so the half-way rounding matches on negative
/// inputs too. With a zero quantum (C's `hz <= 0` path) the value is returned
/// unchanged rather than dividing by zero.
pub fn quantize_to_sleep_quantum(seconds: f64) -> f64 {
    let quantum = thread_sleep_quantum();
    if quantum <= 0.0 || !seconds.is_finite() {
        return seconds;
    }
    let ticks = seconds / quantum;
    let nint = if ticks > 0.0 {
        (ticks + 0.5).trunc()
    } else {
        (ticks - 0.5).trunc()
    };
    quantum * nint
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
