//! Shared alarm-range low-pass filter (`AFTC`/`AFVL`).
//!
//! EPICS records `ai`, `calc`, `longin`, `int64in` and `mbbi` carry an
//! optional alarm filter: when `AFTC > 0` the integer alarm *range* is
//! run through an exponential low-pass so a momentary excursion does not
//! raise (or clear) a limit alarm until the signal has stayed in the new
//! range for roughly `AFTC` seconds. The accumulator persists in `AFVL`.
//!
//! Provenance: the algorithm is by Eric Norum, implemented by Bernd
//! Schoeneburg at the 2009 EPICS Codeathon (epics-base commits
//! `0af48f5a2` and `824d37811` "add the alarm filter for ai, calc,
//! longin, mbbi type records"; later extended to `int64in`). It was
//! **never** part of `biRecord` — `biRecord.c` has no `AFTC`/`AFVL`.

use std::time::SystemTime;

/// Apply the `AFTC` alarm-range low-pass filter.
///
/// Mirrors the C `checkAlarms` filter block in `aiRecord.c:355-401`,
/// `longinRecord.c:310-356`, `int64inRecord.c:303-349` and
/// `mbbiRecord.c:319-336`. `raw_alarm` is the unfiltered value driven
/// into the filter — the integer alarm *range* (`0`=soft … `1`=lolo,
/// `2`=low, `3`=normal, `4`=high, `5`=hihi) for the analog/integer-input
/// records, or the per-state severity for `mbbi`. Returns
/// `(filtered_alarm, new_afvl)`; the caller stores `new_afvl` back into
/// `AFVL` and re-derives severity from `filtered_alarm`.
///
/// Algorithm (C `aiRecord.c:355-401`):
/// ```text
/// afvl = prec->afvl;
/// if (afvl == 0)                         /* seed: pass the raw range */
///     afvl = (double) alarmRange;
/// else {                                 /* exponential low-pass */
///     alpha = aftc / (t + aftc);         /* t = secs since last cycle */
///     afvl  = alpha*afvl
///           + ((afvl > 0) ? (1-alpha) : (alpha-1)) * alarmRange;
///     if (afvl - floor(afvl) > THRESHOLD)   /* THRESHOLD = 0.6321 */
///         afvl = -afvl;                  /* reverse rounding (hysteresis) */
///     alarmRange = abs((int) floor(afvl));
/// }
/// prec->afvl = afvl;
/// ```
/// The *sign* of `afvl` encodes the rounding direction, giving the
/// filter hysteresis: `afvl > 0` rounds `floor()` toward a lower alarm
/// range, `afvl < 0` toward a higher one. `THRESHOLD = 0.6321` (≈ 1−1/e)
/// is defined per record (`aiRecord.c:47`, `longinRecord.c:45`,
/// `int64inRecord.c:44`, `mbbiRecord.c:44`).
pub fn aftc_filter(
    raw_alarm: u16,
    aftc: f64,
    afvl_in: f64,
    time_last: SystemTime,
    time_now: SystemTime,
) -> (u16, f64) {
    // C `aiRecord.c:47` — `#define THRESHOLD 0.6321` (≈ 1 - 1/e).
    const THRESHOLD: f64 = 0.6321;
    // C `aiRecord.c:356` — `afvl = 0;`. When `aftc <= 0` the filter is
    // disabled: C never enters the `if (aftc > 0)` block, so
    // `prec->afvl` is assigned the local `afvl` which is still 0, and
    // the raw `alarmRange` is what `recGblSetSevr` sees.
    if aftc <= 0.0 {
        return (raw_alarm, 0.0);
    }
    // C `aiRecord.c:360-362` — `afvl = prec->afvl; if (afvl == 0) afvl =
    // (double) alarmRange;`. Seed branch: the accumulator is loaded with
    // the raw range and `alarmRange` is left unchanged this cycle.
    if afvl_in == 0.0 {
        return (raw_alarm, raw_alarm as f64);
    }
    // C `aiRecord.c:364-377` — exponential smoothing of the integer
    // range with a signed contribution and a fold-back on the fractional
    // part.
    let dt = time_now
        .duration_since(time_last)
        .unwrap_or_default()
        .as_secs_f64();
    let alpha = aftc / (dt + aftc);
    // `afvl = alpha*afvl + ((afvl>0) ? (1-alpha) : (alpha-1)) * alarmRange`
    let mut afvl = alpha * afvl_in
        + if afvl_in > 0.0 {
            1.0 - alpha
        } else {
            alpha - 1.0
        } * (raw_alarm as f64);
    // `if (afvl - floor(afvl) > THRESHOLD) afvl = -afvl;`
    if afvl - afvl.floor() > THRESHOLD {
        afvl = -afvl;
    }
    // `alarmRange = abs((int)floor(afvl));`
    let alarm = afvl.floor().abs() as u16;
    (alarm, afvl)
}
