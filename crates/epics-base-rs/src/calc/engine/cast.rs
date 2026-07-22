//! The double→integer casts the three calc dialects perform, in one place.
//!
//! Every integer operation in an EPICS calc engine starts by casting a `double`
//! stack cell to an integer, and the three dialects do **not** use the same
//! cast. Keeping one module as the owner is what stops a cast from being copied
//! between dialects, which is how the port ended up running base's `d2i` inside
//! sCalc and aCalc, neither of which has that macro at all:
//!
//! | dialect | bit / shift ops | MODULO / NINT |
//! |---|---|---|
//! | base `calcPerform.c` | `d2i` / `d2ui` (:324-325, :329-366) | `d2i` (MODULO :176-190, NINT :313-317) since `669a25697` / PR #925 |
//! | sCalc `sCalcPerform.c` | plain `(long)` (:578-631, :725) | `c_long` — `(long)` (MODULO :1121, NINT :730) |
//! | aCalc `aCalcPerform.c` | plain `(int)` (:907, :1355-1357, :1424-1427) | MODULO `c_int` — `(int)` (:661, :685, :711); NINT `c_long` — `(long)` (:839, :1096) |
//!
//! MODULO and NINT share the single `nint`/`imod` owners, but each engine
//! passes its OWN dialect's narrowing — one logic, several narrowings. The C
//! engines have genuinely different integer widths, so the port mirrors each
//! rather than forcing one; sCalc's `(long)` even loses less than base's `d2i`
//! (3e9 survives 64-bit intact). Note aCalc is internally inconsistent: its NINT
//! is `(long)` (64-bit) but its MODULO is `(int)` (32-bit), so the array engine
//! passes `c_long` to `nint` and `c_int` to `imod`. The zero-divisor
//! disposition and the `-1 → 0` guard are uniform (CBUG-A2/A1).
//!
//! `d2i`/`d2ui` are *not* general truncating casts: they route a
//! non-negative double through `epicsUInt32` first, so `3e9` becomes
//! `-1294967296` (bit pattern preserved) instead of overflowing. That
//! reinterpretation is exactly what base wants for a bitwise operand and
//! exactly what C's plain cast does *not* do.

/// C `d2i` (`calcPerform.c:324`):
/// `((x)<0 ? (epicsInt32)(x) : (epicsInt32)(epicsUInt32)(x))`.
///
/// A non-negative double goes through `epicsUInt32`, so the full 32-bit
/// pattern survives and bit 31 lands as the sign bit — `d2i(3e9)` is
/// `-1294967296`, not an overflow. Base uses this for BIT_OR/AND/XOR/NOT, the
/// shifts, and — since `669a25697` / PR #925 — NINT and MODULO too.
#[inline]
pub(crate) fn d2i(x: f64) -> i32 {
    if x < 0.0 {
        f64_to_i32_wrap(x)
    } else {
        f64_to_u32_wrap(x) as i32
    }
}

/// C `d2ui` (`calcPerform.c:325`):
/// `((x)<0 ? (epicsUInt32)(epicsInt32)(x) : (epicsUInt32)(x))`.
/// Base's logical right shift (`>>>`, RIGHT_SHIFT_LOGIC) is its only user.
#[inline]
pub(crate) fn d2ui(x: f64) -> u32 {
    if x < 0.0 {
        f64_to_i32_wrap(x) as u32
    } else {
        f64_to_u32_wrap(x)
    }
}

/// C's plain `(int)` / `(epicsInt32)` cast of a double.
///
/// In range it truncates toward zero. Out of range (and for NaN/±Inf) the C
/// standard calls it **undefined**, so a compiled C IOC is not single-valued:
/// x86-64's `cvttsd2si` stores the "integer indefinite" `INT32_MIN`, while
/// aarch64's `fcvtzs` saturates to `INT32_MAX` and sends NaN to 0. Since UB is
/// not a contract, the port takes the clean value and **saturates** — see
/// [`crate::types::c_cast`] for the decision (CBUG-E2) and the disassembly.
///
/// Either way it differs from [`d2i`]'s wrap, which is the bug this module's
/// split exists to prevent: `d2i` routes a non-negative double through
/// `epicsUInt32` first, and that reinterpretation *is* well defined for
/// anything inside `u32`.
///
/// The cast itself is not calc's — it is the same bare C cast `dbConvert.c`
/// performs on a `DBF_DOUBLE` → `DBF_LONG` put — so it is owned by
/// [`crate::types::c_cast`] and merely named for calc here.
#[inline]
pub(crate) fn c_int(x: f64) -> i32 {
    crate::types::c_cast::f64_to_i32(x)
}

/// C `myNINT` — `sCalcPerform.c:40` and `aCalcPerform.c:50`, byte-identical:
///
/// ```c
/// #define myNINT(a) ((int)((a) >= 0 ? (a)+0.5 : (a)-0.5))
/// ```
///
/// Round half away from zero, **and then cast to `int`** — the narrowing is
/// INSIDE the macro, which is the whole point of it living here. A caller that
/// takes `myNINT`'s value into a `long` gets an already-narrowed `int`
/// sign-extended, not a fresh 64-bit conversion of the double.
///
/// The port used to have two copies of this and neither narrowed like C: sCalc's
/// returned a `double` (so each of its call sites invented its own narrowing —
/// `as i64` wrapped, `as i32` saturated, and the two disagreed *bitwise* on the
/// same input), and aCalc's used Rust's `as i32`, which saturates where C's
/// `cvttsd2si` yields the indefinite value. One function, one narrowing.
///
/// Compiled (`gcc -O0`, x86-64, runtime operand — a *constant* operand is folded
/// by gcc to `INT32_MAX` instead, which is why this must be probed at runtime):
/// `myNINT(3e9)` = `myNINT(-3e9)` = `myNINT(1e18)` = `myNINT(NaN)` =
/// `-2147483648`; `myNINT(2.5)` = 3; `myNINT(-2.5)` = -3.
#[inline]
pub(crate) fn my_nint(a: f64) -> i32 {
    c_int(if a >= 0.0 { a + 0.5 } else { a - 0.5 })
}

/// C's plain `(long)` cast of a double on LP64 (what sCalc's operators use).
/// Same story as [`c_int`], one width up: x86-64 `cvttsd2si` with a 64-bit
/// destination yields `INT64_MIN` for NaN and for anything out of range.
#[inline]
pub(crate) fn c_long(x: f64) -> i64 {
    crate::types::c_cast::f64_to_i64(x)
}

/// `NINT` — round half away from zero, then narrow with the caller's dialect
/// cast (`narrow`). One body, per-dialect narrowing: the numeric engine passes
/// [`d2i`] (base's fixed `calcPerform.c:313-317`, `669a25697`/PR #925), and both
/// sCalc (`sCalcPerform.c:730`) and aCalc (`aCalcPerform.c:839,1096`) pass
/// [`c_long`] — BOTH synApps engines narrow NINT with `(long)`, 64-bit — each
/// mirroring THAT dialect's own C.
///
/// C shape, identical in all three but for the cast:
///
/// ```c
/// top = top >= 0 ? top + 0.5 : top - 0.5;
/// *ptop = <narrow>(top);
/// ```
///
/// So `NINT(3e9)` is `d2i(3e9) = -1294967296` in base (the u32 reinterpretation),
/// but `(long)3e9 = 3000000000` in both sCalc and aCalc (64-bit, no loss).
/// Inside each width's range the three agree and are bit-identical to their C.
#[inline]
pub(crate) fn nint(x: f64, narrow: impl Fn(f64) -> i64) -> f64 {
    let rounded = if x >= 0.0 { x + 0.5 } else { x - 0.5 };
    narrow(rounded) as f64
}

/// `MODULO` — narrow both operands with the caller's dialect cast (`narrow`),
/// then take the integer remainder. Returns `None` when the divisor narrows to
/// zero. One body, three narrowings — see [`nint`] for the per-dialect casts.
///
/// C shape, identical in all three engines but for the cast width (base's fixed
/// `calcPerform.c:176-190`; sCalc `sCalcPerform.c:558-563`/`:1102-1110`; aCalc
/// `aCalcPerform.c:645-703`):
///
/// ```c
/// itop = <narrow>(top);                  /* divisor */
/// if (itop == 0)       *ptop = epicsNAN; /* caller's zero rule */
/// else if (itop == -1) *ptop = 0;        /* x % -1 == 0, dodges INT_MIN % -1 */
/// else                 *ptop = <narrow>(*ptop) % itop;
/// ```
///
/// `None` signals the divisor narrowed to 0 so the caller applies its engine's
/// zero-divisor rule (base `NaN`, sCalc error, aCalc `myMAXFLOAT`) — exactly the
/// three arms C's own `case MODULO` splits into. A `-1` divisor returns `0`,
/// both mathematically correct (`x % -1 == 0` for every integer) and the value
/// all three C engines now define (calc PR #38 + base `fd64a84aa`) to avoid the
/// undefined `INT_MIN % -1` that x86 `idiv` traps as SIGFPE.
#[inline]
pub(crate) fn imod(a: f64, b: f64, narrow: impl Fn(f64) -> i64) -> Option<f64> {
    let divisor = narrow(b);
    if divisor == 0 {
        None
    } else if divisor == -1 {
        // x % -1 == 0; also the one input the raw `%` leaves UB (INT_MIN % -1),
        // which x86 `idiv` traps as SIGFPE — never reach the `%` below for it.
        Some(0.0)
    } else {
        Some((narrow(a) % divisor) as f64)
    }
}

/// `(epicsInt32)x` where the value is already known to be in `epicsUInt32`
/// range — the tail of `d2i`/`d2ui`, i.e. a modular reduction, NOT a C cast.
/// Private on purpose: an operator that wants a C cast wants [`c_int`].
#[inline]
fn f64_to_i32_wrap(x: f64) -> i32 {
    if x.is_nan() {
        return 0;
    }
    let m = x.trunc().rem_euclid(4294967296.0);
    m as u64 as u32 as i32
}

#[inline]
fn f64_to_u32_wrap(x: f64) -> u32 {
    if x.is_nan() {
        return 0;
    }
    let m = x.trunc().rem_euclid(4294967296.0);
    m as u64 as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the split: on the values where they disagree, `d2i`
    /// and a C cast are different numbers. They agree everywhere inside
    /// `[i32::MIN, i32::MAX]`, which is why the bug hid — `A % 7` only
    /// diverges once A climbs past 2^31.
    #[test]
    fn d2i_and_c_int_agree_in_range_and_diverge_outside_it() {
        for x in [0.0, 1.0, -1.0, 7.9, -7.9, 2147483647.0, -2147483648.0] {
            assert_eq!(d2i(x), c_int(x), "for {x}");
        }
        // 3e9 fits in epicsUInt32, so d2i reinterprets it as a negative int32.
        // This is base's macro and is NOT the UB cast — it stays exactly as C.
        assert_eq!(d2i(3e9), -1_294_967_296);
        // The plain cast has no such route: 3e9 is out of `i32` range, and there
        // it saturates (CBUG-E2) rather than taking x86's undefined `INT32_MIN`.
        assert_eq!(c_int(3e9), i32::MAX);
        assert_eq!(c_long(3e9), 3_000_000_000);
    }

    /// The narrowing is inside C's macro — that placement is what this pins, and
    /// it is unchanged. The out-of-range VALUE is the saturating one (CBUG-E2),
    /// not x86's `INT32_MIN`; an aarch64 IOC agrees with us.
    #[test]
    fn my_nint_rounds_then_casts_at_c_width() {
        assert_eq!(my_nint(2.5), 3);
        assert_eq!(my_nint(-2.5), -3);
        assert_eq!(my_nint(2.4), 2);
        assert_eq!(my_nint(-2.4), -2);
        assert_eq!(my_nint(0.0), 0);
        // Out of int32 range: saturate at the bound of the sign that overflowed.
        assert_eq!(my_nint(3e9), i32::MAX);
        assert_eq!(my_nint(-3e9), i32::MIN);
        assert_eq!(my_nint(1e18), i32::MAX);
        assert_eq!(my_nint(f64::NAN), 0);
        // The rounding happens BEFORE the cast, so INT32_MAX still fits.
        assert_eq!(my_nint(2147483647.0), i32::MAX);
        // Still not d2i: base's bitwise wrap would say -1294967296.
        assert_ne!(my_nint(3e9), d2i(3e9));
    }

    #[test]
    fn d2i_and_d2ui_are_the_base_macros() {
        assert_eq!(d2ui(3e9), 3_000_000_000);
        assert_eq!(d2i(-1.0), -1);
        assert_eq!(d2ui(-1.0), 0xFFFF_FFFF);
    }

    /// The plain casts saturate, and NaN converts to 0 (CBUG-E2). x86-64's
    /// `cvttsd2si` would answer `INT32_MIN`/`INT64_MIN` to every line below;
    /// that value is UB and we do not store it.
    #[test]
    fn c_casts_saturate_and_send_nan_to_zero() {
        assert_eq!(c_int(f64::NAN), 0);
        assert_eq!(c_int(f64::INFINITY), i32::MAX);
        assert_eq!(c_int(f64::NEG_INFINITY), i32::MIN);
        assert_eq!(c_long(f64::NAN), 0);
        assert_eq!(c_long(1e300), i64::MAX);
        // The 2^63 boundary: representable as f64 and NOT representable as i64.
        assert_eq!(c_long(9223372036854775808.0), i64::MAX);
        assert_eq!(c_long(-9223372036854775808.0), i64::MIN);
    }
}
