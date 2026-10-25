//! `scanProg.st` (program `scanProgress`), compiled by snc-rs from
//! `build.rs` and run on `epics-seq-rs`.
//!
//! The escaped C is the time keeping: `%%` statements that stamp the
//! epoch-second globals and format them into the progress strings, and the
//! `%{ }%` block's helpers. `calc_cpt` and `allStopped` read the scan
//! globals, which under `-r` are the block `seqg::seqg_static_vars()`
//! returns.

use epics_seq_rs::c::*;
use epics_seq_rs::{SeqString, SeqVar};

macro_rules! seqg_escaped {
    (r#"#include <string.h>"#) => {};
    (r#"#include <epicsThread.h>"#) => {};
    (r#"#include <epicsTime.h>"#) => {};
    (r#"#include <time.h>"#) => {};
    (r#"#include <limits.h>"#) => {};
    (r#"static void clocks2str(long isec, char *str);"#) => {};
    (r#"static void formatTimeStr(char *tStr, int tStrLen, time_t final, int full);"#) => {};
    (r#"static long calc_cpt(void);"#) => {};
    (r#"static int allStopped(void);"#) => {};
    ($env:ident, r#"startTime = time(0L);"#) => {
        $env.var().startTime.set(time())
    };
    ($env:ident, r#"pauseStart = time(0L);"#) => {
        $env.var().pauseStart.set(time())
    };
    ($env:ident, r#"now = time(0L);"#) => {
        $env.var().now.set(time())
    };
    ($env:ident, r#"formatTimeStr(startingTimeStr,40,startTime,1);"#) => {
        formatTimeStr(
            &$env.var().startingTimeStr,
            40,
            $env.var().startTime.get(),
            1,
        )
    };
    ($env:ident, r#"formatTimeStr(endingTimeStr,40,predictEnd,0);"#) => {
        formatTimeStr(
            &$env.var().endingTimeStr,
            40,
            $env.var().predictEnd.get(),
            0,
        )
    };
    ($env:ident, r#"formatTimeStr(endingTimeStr,40,now,1);"#) => {
        formatTimeStr(&$env.var().endingTimeStr, 40, $env.var().now.get(), 1)
    };
    ($env:ident, r#"fractionDone = (double)cpt/(double)npts;"#) => {{
        let v = $env.var();
        v.fractionDone
            .set(f64::from(v.cpt.get()) / f64::from(v.npts.get()))
    }};
    // The static block rather than `$env`, so the unit test expands it bare.
    ($env:ident, r#"{ double r = (1.0-fractionDone)/fractionDone * (double)(now-startTime-pauseSec); remainingSec = r > INT_MAX ? INT_MAX : r < INT_MIN ? INT_MIN : (int)r; }"#) => {{
        let v = seqg::seqg_static_vars();
        let f = v.fractionDone.get();
        let active = v.now.get() - v.startTime.get() - i64::from(v.pauseSec.get());
        // `as` saturates, as the .st clamps.
        v.remainingSec.set(((1.0 - f) / f * active as f64) as i32)
    }};
    ($env:ident, r#"clocks2str(remainingSec,tStr);"#) => {
        clocks2str($env.var().remainingSec.get().into(), &$env.var().tStr)
    };
    ($env:ident, r#"clocks2str(hungSec,tStr);"#) => {
        clocks2str($env.var().hungSec.get().into(), &$env.var().tStr)
    };
    ($env:ident, r#"clocks2str(pauseSec,tStr);"#) => {
        clocks2str($env.var().pauseSec.get().into(), &$env.var().tStr)
    };
    ($env:ident, r#"clocks2str(now-startTime,tStr);"#) => {{
        let v = $env.var();
        clocks2str(v.now.get() - v.startTime.get(), &v.tStr)
    }};
    ($env:ident, r#"clocks2str(now-startTime-pauseSec,tStr);"#) => {{
        let v = $env.var();
        clocks2str(
            v.now.get() - v.startTime.get() - i64::from(v.pauseSec.get()),
            &v.tStr,
        )
    }};
    ($env:ident, r#"clocks2str(now-pauseStart,tStr);"#) => {{
        let v = $env.var();
        clocks2str(v.now.get() - v.pauseStart.get(), &v.tStr)
    }};
    ($env:ident, r#"clocks2str(now-pauseStart+pauseSec,tStr1);"#) => {{
        let v = $env.var();
        clocks2str(
            v.now.get() - v.pauseStart.get() + i64::from(v.pauseSec.get()),
            &v.tStr1,
        )
    }};
    (r#"static long calc_cpt(void) { long lcpt; long nBelow; lcpt = cpt1; if (scanDim<2) return lcpt; nBelow = npts1; lcpt += cpt2 * nBelow; if (scanDim<3) return lcpt; nBelow *= npts2; lcpt += cpt3 * nBelow; if (scanDim<4) return lcpt; nBelow *= npts3; lcpt += cpt4*nBelow; return lcpt; } static void clocks2str(long isec, char *str) { int hh,mm,ss; int sign=1; if (isec>32000000) { strcpy(str,"infinite"); return; } else if (isec<-32000000) { strcpy(str,"-infinite"); return; } else if (isec<0) { isec *= -1; sign = -1; } ss = isec % 60; isec /= 60; mm = isec % 60; hh = sign * isec / 60; sprintf(str,"%02d:%02d:%02d",hh,mm,ss); return; } static void formatTimeStr( char *tStr, int tStrLen, time_t final, int full) { time_t now; struct tm *tm1; int isToday=0; int isTomorrow=0; int isYesterday=0; int hh,mm,ss; time_t final_t = (time_t)final; if (!full) { tm1 = localtime(&final_t); hh = tm1->tm_hour; mm = tm1->tm_min; ss = tm1->tm_sec; now = time(0L); tm1 = localtime(&now); tm1->tm_hour = hh; tm1->tm_min = mm; tm1->tm_sec = ss; isToday = (final_t==mktime(tm1)); if (!isToday) { tm1->tm_mday++; isTomorrow = (final_t==mktime(tm1)); } if (!isToday && !isTomorrow) { tm1->tm_mday -= 2; isYesterday = (final_t==mktime(tm1)); } } tm1 = localtime(&final_t); if (isToday) strftime(tStr,tStrLen, "%H:%M:%S Today (%A)",tm1); else if (isTomorrow) strftime(tStr,tStrLen, "%H:%M:%S Tomorrow (%A)",tm1); else if (isYesterday) strftime(tStr,tStrLen, "%H:%M:%S Yesterday (%A)",tm1); else strftime(tStr,tStrLen, "%H:%M:%S  (%a) %b %d, %Y",tm1); return; } static int allStopped(void) { int stopCount; stopCount = (pause1 || !busy1) ? 1 : 0; stopCount += (scanDim<2) || (pause2 || !busy2) ? 2 : 0; stopCount += (scanDim<3) || (pause3 || !busy3) ? 4 : 0; stopCount += (scanDim<4) || (pause4 || !busy4) ? 8 : 0; return (stopCount==15); }"#) => {};
}

epics_seq_rs::include_program!("scanProg");

/// `calc_cpt`: points done across the `scanDim` nested scans, each pass of
/// an outer scan worth every point of the scans inside it.
fn calc_cpt() -> i64 {
    let v = seqg::seqg_static_vars();
    let scan_dim = v.scanDim.get();
    let mut lcpt = i64::from(v.cpt1.get());
    if scan_dim < 2 {
        return lcpt;
    }
    let mut n_below = i64::from(v.npts1.get());
    lcpt = lcpt.wrapping_add(i64::from(v.cpt2.get()).wrapping_mul(n_below));
    if scan_dim < 3 {
        return lcpt;
    }
    n_below = n_below.wrapping_mul(v.npts2.get().into());
    lcpt = lcpt.wrapping_add(i64::from(v.cpt3.get()).wrapping_mul(n_below));
    if scan_dim < 4 {
        return lcpt;
    }
    n_below = n_below.wrapping_mul(v.npts3.get().into());
    lcpt.wrapping_add(i64::from(v.cpt4.get()).wrapping_mul(n_below))
}

/// `clocks2str`: seconds as `hh:mm:ss`, `infinite` past a year and nine
/// days. The sign rides on the hours, so under an hour it is lost.
fn clocks2str(isec: i64, str: impl CDst) {
    if isec > 32_000_000 {
        return strcpy(str, "infinite");
    }
    if isec < -32_000_000 {
        return strcpy(str, "-infinite");
    }
    let (sign, isec) = if isec < 0 { (-1, -isec) } else { (1, isec) };
    let ss = isec % 60;
    let isec = isec / 60;
    let mm = isec % 60;
    let hh = sign * isec / 60;
    strcpy(str, format!("{hh:02}:{mm:02}:{ss:02}").as_str());
}

/// `formatTimeStr` at the time `time(NULL)` gives.
#[allow(non_snake_case)]
fn formatTimeStr(t_str: &SeqVar<SeqString>, t_str_len: usize, final_t: i64, full: i32) {
    format_time_at(t_str, t_str_len, final_t, full, time());
}

/// `formatTimeStr`: `final_t` in local time, named Today, Tomorrow or
/// Yesterday unless `full`. A day is named only when `mktime` turns
/// `final_t`'s time of day on that date, in `now`'s DST, back into
/// `final_t`; so across a DST change neither date is named.
fn format_time_at(t_str: &SeqVar<SeqString>, t_str_len: usize, final_t: i64, full: i32, now: i64) {
    let (mut is_today, mut is_tomorrow, mut is_yesterday) = (false, false, false);
    if full == 0 {
        let at = ctime::localtime(final_t);
        let mut tm1 = ctime::localtime(now);
        tm1.tm_hour = at.tm_hour;
        tm1.tm_min = at.tm_min;
        tm1.tm_sec = at.tm_sec;
        is_today = final_t == ctime::mktime(&mut tm1);
        if !is_today {
            tm1.tm_mday += 1;
            is_tomorrow = final_t == ctime::mktime(&mut tm1);
        }
        if !is_today && !is_tomorrow {
            tm1.tm_mday -= 2;
            is_yesterday = final_t == ctime::mktime(&mut tm1);
        }
    }
    let fmt = if is_today {
        c"%H:%M:%S Today (%A)"
    } else if is_tomorrow {
        c"%H:%M:%S Tomorrow (%A)"
    } else if is_yesterday {
        c"%H:%M:%S Yesterday (%A)"
    } else {
        c"%H:%M:%S  (%a) %b %d, %Y"
    };
    let text = ctime::strftime(t_str_len, fmt, &ctime::localtime(final_t));
    strcpy(t_str, text.as_slice());
}

/// `allStopped`: every scan up to `scanDim` is paused or not busy.
#[allow(non_snake_case)]
fn allStopped() -> i32 {
    let v = seqg::seqg_static_vars();
    let scan_dim = v.scanDim.get();
    let stopped = |pause: &SeqVar<i16>, busy: &SeqVar<i16>| pause.get() != 0 || busy.get() == 0;
    let mut stop_count = if stopped(&v.pause1, &v.busy1) { 1 } else { 0 };
    stop_count += if scan_dim < 2 || stopped(&v.pause2, &v.busy2) {
        2
    } else {
        0
    };
    stop_count += if scan_dim < 3 || stopped(&v.pause3, &v.busy3) {
        4
    } else {
        0
    };
    stop_count += if scan_dim < 4 || stopped(&v.pause4, &v.busy4) {
        8
    } else {
        0
    };
    i32::from(stop_count == 15)
}

/// `time(NULL)`.
fn time() -> i64 {
    ctime::time()
}

/// The `<time.h>` calls `formatTimeStr` makes.
mod ctime {
    use std::ffi::CStr;

    pub type Tm = libc::tm;

    /// `localtime`, into a `tm` of this call's own. C bug not copied: on
    /// Windows a time before 1970 gets a null `tm`, which C dereferences;
    /// here it is zeros.
    pub fn localtime(t: i64) -> Tm {
        let t = t as libc::time_t;
        // SAFETY: `tm` is integers plus, on some systems, a `tm_zone`
        // pointer, for which null is valid.
        let mut tm: Tm = unsafe { std::mem::zeroed() };
        #[cfg(unix)]
        // SAFETY: both pointers are to live locals.
        unsafe {
            libc::localtime_r(&t, &mut tm);
        }
        #[cfg(windows)]
        // SAFETY: both pointers are to live locals.
        unsafe {
            libc::localtime_s(&mut tm, &t);
        }
        tm
    }

    #[cfg(windows)]
    unsafe extern "C" {
        #[link_name = "_mktime64"]
        fn c_mktime(tm: *mut Tm) -> libc::time_t;
        #[link_name = "strftime"]
        fn c_strftime(
            s: *mut libc::c_char,
            max: libc::size_t,
            format: *const libc::c_char,
            tm: *const Tm,
        ) -> libc::size_t;
        fn _set_thread_local_invalid_parameter_handler(
            handler: Option<InvalidParameterHandler>,
        ) -> Option<InvalidParameterHandler>;
    }
    #[cfg(unix)]
    use libc::{mktime as c_mktime, strftime as c_strftime};

    #[cfg(windows)]
    type InvalidParameterHandler =
        unsafe extern "C" fn(*const u16, *const u16, *const u16, u32, usize);
    #[cfg(windows)]
    unsafe extern "C" fn returning(_: *const u16, _: *const u16, _: *const u16, _: u32, _: usize) {}

    /// `mktime`, which normalises `tm` in place.
    pub fn mktime(tm: &mut Tm) -> i64 {
        // SAFETY: `tm` is a live, initialised struct.
        unsafe { c_mktime(tm) as i64 }
    }

    /// `strftime` into a `max`-byte buffer: nothing when the result does
    /// not fit.
    pub fn strftime(max: usize, format: &CStr, tm: &Tm) -> Vec<u8> {
        let mut buffer = vec![0u8; max];
        // UCRT's strftime calls the invalid parameter handler on that zero
        // `tm`, and the default handler ends the IOC; under one that
        // returns, strftime returns 0.
        #[cfg(windows)]
        // SAFETY: the handler only returns; the old one is put back below.
        let previous = unsafe { _set_thread_local_invalid_parameter_handler(Some(returning)) };
        // SAFETY: `format` is NUL-terminated and `buffer` holds the `max`
        // bytes strftime may write.
        let length = unsafe { c_strftime(buffer.as_mut_ptr().cast(), max, format.as_ptr(), tm) };
        #[cfg(windows)]
        // SAFETY: as above.
        unsafe {
            _set_thread_local_invalid_parameter_handler(previous)
        };
        buffer.truncate(length);
        buffer
    }

    /// `time(NULL)`.
    pub fn time() -> i64 {
        // SAFETY: a null argument is allowed.
        unsafe { libc::time(std::ptr::null_mut()) as i64 }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::c_char;
    use std::sync::Mutex;

    use epics_seq_rs::{ProgOptions, VarModel, Zeroed};

    use super::*;

    /// `calc_cpt` and `allStopped` work on the program's one static block.
    static BLOCK: Mutex<()> = Mutex::new(());

    /// The zone every local-time case runs in. US Eastern, DST from the
    /// second Sunday in March (2026-03-08) to the first in November.
    fn eastern() {
        // SAFETY: this module's only time zone is set here, before any libc
        // time function runs in these tests.
        unsafe { std::env::set_var("TZ", "EST5EDT,M3.2.0,M11.1.0") };
    }

    fn text<B: CSrc + CBuf + Zeroed>(f: impl FnOnce(&SeqVar<B>)) -> String {
        let v = <SeqVar<B>>::zeroed();
        f(&v);
        String::from_utf8(v.to_c_vec()).unwrap()
    }

    /// scanProg.st:1, compiled with snc's defaults.
    #[test]
    fn the_program_is_the_st_one() {
        assert_eq!(scanProgress.validate(), Ok(()));
        assert_eq!(scanProgress.params, "S=34ide:, P=34ide:scanProgress:");
        assert_eq!(
            scanProgress.options,
            ProgOptions::new(ProgOptions::CONN | ProgOptions::NEWEF)
        );
        assert!(matches!(scanProgress.vars, VarModel::Static(_)));
    }

    /// Against scanProg.st's own clocks2str, compiled with gcc.
    #[test]
    fn clocks2str_is_hh_mm_ss_with_the_sign_on_the_hours() {
        let c = |isec| text::<[c_char; 100]>(|v| clocks2str(isec, v));
        assert_eq!(c(0), "00:00:00");
        assert_eq!(c(3725), "01:02:05");
        assert_eq!(c(-3725), "-1:02:05");
        // `hh = sign * isec / 60` is zero, unsigned, under an hour.
        assert_eq!(c(-59), "00:00:59");
        assert_eq!(c(360_000), "100:00:00");
        assert_eq!(c(32_000_000), "8888:53:20");
        assert_eq!(c(32_000_001), "infinite");
        assert_eq!(c(-32_000_001), "-infinite");
    }

    fn scans(scan_dim: i32, cpt: [i32; 4], npts: [i32; 4], pause: [i16; 4], busy: [i16; 4]) {
        let v = seqg::seqg_static_vars();
        v.scanDim.set(scan_dim);
        for (var, x) in [&v.cpt1, &v.cpt2, &v.cpt3, &v.cpt4].into_iter().zip(cpt) {
            var.set(x);
        }
        for (var, x) in [&v.npts1, &v.npts2, &v.npts3, &v.npts4]
            .into_iter()
            .zip(npts)
        {
            var.set(x);
        }
        for (var, x) in [&v.pause1, &v.pause2, &v.pause3, &v.pause4]
            .into_iter()
            .zip(pause)
        {
            var.set(x);
        }
        for (var, x) in [&v.busy1, &v.busy2, &v.busy3, &v.busy4]
            .into_iter()
            .zip(busy)
        {
            var.set(x);
        }
    }

    #[test]
    fn calc_cpt_weights_each_outer_point_by_the_points_inside_it() {
        let _block = BLOCK.lock().unwrap();
        let cpt_at = |scan_dim| {
            scans(scan_dim, [4, 2, 1, 1], [10, 5, 3, 2], [0; 4], [0; 4]);
            calc_cpt()
        };
        assert_eq!(cpt_at(1), 4);
        assert_eq!(cpt_at(2), 4 + 2 * 10);
        assert_eq!(cpt_at(3), 4 + 2 * 10 + 50);
        assert_eq!(cpt_at(4), 4 + 2 * 10 + 50 + 150);
        // `scanDim<2` returns scan1's count; `scanDim<4` is passed past 4.
        assert_eq!(cpt_at(0), 4);
        assert_eq!(cpt_at(5), 4 + 2 * 10 + 50 + 150);
    }

    #[test]
    fn all_stopped_ignores_dimensions_past_the_scan() {
        let _block = BLOCK.lock().unwrap();
        let stopped = |scan_dim, pause, busy| {
            scans(scan_dim, [0; 4], [0; 4], pause, busy);
            allStopped()
        };
        let none = [0; 4];
        assert_eq!(stopped(1, none, none), 1);
        assert_eq!(stopped(1, none, [1, 0, 0, 0]), 0);
        assert_eq!(stopped(1, [1, 0, 0, 0], [1, 1, 1, 1]), 1);
        assert_eq!(stopped(2, [1, 0, 0, 0], [1, 1, 0, 0]), 0);
        assert_eq!(stopped(2, [1, 1, 0, 0], [1, 1, 1, 1]), 1);
        assert_eq!(stopped(4, [0, 0, 0, 1], [0, 0, 0, 1]), 1);
    }

    fn format(final_t: i64, full: i32, now: i64) -> String {
        text::<SeqString>(|v| format_time_at(v, 40, final_t, full, now))
    }

    /// Against scanProg.st's own formatTimeStr, compiled with gcc, at
    /// 2026-03-11 12:00 EDT.
    #[test]
    fn format_time_str_names_the_day_against_now() {
        eastern();
        let now = 1_773_244_800;
        let at = |h: i64| now + h * 3600;
        assert_eq!(format(at(2), 0, now), "14:00:00 Today (Wednesday)");
        assert_eq!(format(at(24), 0, now), "12:00:00 Tomorrow (Thursday)");
        assert_eq!(format(at(-24), 0, now), "12:00:00 Yesterday (Tuesday)");
        assert_eq!(format(at(48), 0, now), "12:00:00  (Fri) Mar 13, 2026");
        assert_eq!(format(at(2), 1, now), "14:00:00  (Wed) Mar 11, 2026");
    }

    /// On 2026-03-08, 01:00 EST is the same date as 12:00 EDT and Sunday
    /// 12:00 EDT the day after Saturday 12:00 EST, but `mktime` in `now`'s
    /// DST misses both, so C names neither day. Against the same gcc build.
    #[cfg(unix)]
    #[test]
    fn across_a_dst_change_no_day_is_named() {
        eastern();
        let sunday_noon = 1_772_985_600;
        assert_eq!(
            format(1_772_949_600, 0, sunday_noon),
            "01:00:00  (Sun) Mar 08, 2026"
        );
        assert_eq!(
            format(sunday_noon + 3600, 0, sunday_noon),
            "13:00:00 Today (Sunday)"
        );
        let saturday_noon = sunday_noon - 86_400 + 3600;
        assert_eq!(
            format(sunday_noon, 0, saturday_noon),
            "12:00:00  (Sun) Mar 08, 2026"
        );
    }

    /// A 1000x1000 scan with one point done: past 2148 active seconds the
    /// estimate is past `INT_MAX`, where C's `(int)` was undefined.
    #[test]
    fn remaining_sec_clamps_to_int() {
        let _block = BLOCK.lock().unwrap();
        let remaining = |f, active: i64| {
            let v = seqg::seqg_static_vars();
            v.fractionDone.set(f);
            v.startTime.set(1_000);
            v.pauseSec.set(0);
            v.now.set(1_000 + active);
            seqg_escaped!(
                no_env,
                r#"{ double r = (1.0-fractionDone)/fractionDone * (double)(now-startTime-pauseSec); remainingSec = r > INT_MAX ? INT_MAX : r < INT_MIN ? INT_MIN : (int)r; }"#
            );
            v.remainingSec.get()
        };
        assert_eq!(remaining(0.5, 100), 100);
        assert_eq!(remaining(1e-6, 2_147), 2_146_997_853);
        assert_eq!(remaining(1e-6, 2_148), i32::MAX);
        // A clock stepped back.
        assert_eq!(remaining(1e-6, -2_148), i32::MIN);
    }
}
