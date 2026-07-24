//! aCalc's DERIV and NDERIV (`calcUtil.c:27-74`).
//!
//! Both are the SAME kernel — `deriv()` is literally `nderiv(..., npts=2, ...)`
//! (`:70-74`) — and that kernel is a **sliding QUADRATIC fit**, not a difference
//! formula and not a linear regression:
//!
//! ```c
//! m = 2*npts+1;
//! e = fitpoly(x, y, m, &c, &b, &a, NULL);          /* y = c + b*x + a*x*x */
//! for (j=0; j<m/2+1; j++) d[j] = b + 2*a*x[j];     /* dy/dx = b + 2*a*x   */
//! ```
//!
//! The port had a central difference for DERIV and a least-squares LINE for NDERIV,
//! and read NDERIV's argument as a total window width. It is points PER SIDE:
//! `NDERIV(y,1)` fits 3 points, `NDERIV(y,2)` fits 5 — and `NDERIV(y,2)` is exactly
//! `DERIV(y)`.

use crate::calc::math::fitting;

/// C `deriv` (`calcUtil.c:70-74`) — `nderiv` with `npts = 2`, i.e. a five-point
/// sliding quadratic fit. Compiled C confirms the identity: for `AA=[0,1,10,3,4,5,6,7,20]`,
/// `DERIV(AA)` and `NDERIV(AA,2)` are the same nine numbers.
pub fn deriv(y: &[f64]) -> Option<Vec<f64>> {
    nderiv(y, 2)
}

/// C `nderiv(x, y, n, d, npts, lx)` (`calcUtil.c:27-68`). `npts` is the number of
/// points on EACH SIDE, so the fit width is `m = 2*npts+1`.
///
/// Three blocks, each a quadratic fit whose derivative `b + 2*a*x` is evaluated at
/// the points it covers:
///
/// * the first `m/2+1` points share ONE fit over `y[0..m]`;
/// * each middle point gets its own fit over the `m` points centred on it;
/// * the last `m/2+1` points share ONE fit over `y[n-m..n]`.
///
/// `x` is the ELEMENT INDEX in every aCalc caller, which is why C's mixed use of the
/// global `x[]` in the first and last blocks and the local `lx[]` in the middle one
/// makes no difference: a segment of the index array, rebased to its own start, is
/// again `0, 1, 2, ...`.
///
/// `None` is C's `return(e)` — the fit failed, which for `m < 3` is `fitpoly`'s
/// `n < 3` rejection (`calcUtil.c:270`). Compiled C: `NDERIV(AA,0)` is status -1.
///
/// # The clamp, and the one deliberate deviation
///
/// C clamps `npts` to half the window — `j = myMIN((lastEl-firstEl)/2, j)`,
/// `aCalcPerform.c:601` — but only in the NDERIV opcode, not in `nderiv` itself, and
/// `deriv()` passes its fixed `npts = 2` straight through. So C's DERIV on a window
/// of fewer than 5 points fits `m = 5` points out of `n < 5`: it reads past the
/// window, and its last block indexes `x[n-m]` with `n-m` NEGATIVE — before the
/// start of the array. That is C undefined behaviour, and the compiled reference
/// duly answers garbage (`DERIV(AA[0,3])` -> [7.15, -0.23, -3.92, 7.15]).
///
/// The clamp therefore lives HERE, in the kernel, where no caller can bypass it:
/// DERIV of a short window becomes NDERIV of that window with the same clamp, which
/// is the answer C's own NDERIV gives. For windows of 5 points or more — every case
/// C defines — the clamp is a no-op and the two agree exactly.
pub fn nderiv(y: &[f64], npts: i64) -> Option<Vec<f64>> {
    let n = y.len();
    // C `aCalcPerform.c:601`, hoisted into the kernel: at most half the window on
    // each side, so `m <= n` holds by construction.
    let npts = npts.min((n as i64 - 1) / 2);
    let m = 2 * npts + 1;
    if m < 3 {
        // `fitpoly` needs three points for three coefficients (`calcUtil.c:270`).
        return None;
    }
    let m = m as usize;
    let half = m / 2;

    // The x of every fit: a rebased segment of the index array is just 0..m.
    let x: Vec<f64> = (0..m).map(|i| i as f64).collect();
    let mut d = vec![0.0; n];

    // First m/2+1 points — one fit, evaluated at each of them (`:34-43`).
    let (_, b, a) = fitting::fitpoly(&x, &y[..m], None)?;
    for (j, dj) in d[..=half].iter_mut().enumerate() {
        *dj = b + 2.0 * a * j as f64;
    }

    // Middle points — one fit each, centred (`:45-57`).
    for i in half + 1..n - (half + 1) {
        let (_, b, a) = fitting::fitpoly(&x, &y[i - half..i - half + m], None)?;
        d[i] = b + 2.0 * a * half as f64;
    }

    // Last m/2+1 points — one fit, evaluated at their offsets within it (`:59-67`).
    let (_, b, a) = fitting::fitpoly(&x, &y[n - m..], None)?;
    for j in 0..=half {
        d[n - (half + 1) + j] = b + 2.0 * a * (j + half) as f64;
    }
    Some(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A quadratic is reproduced EXACTLY by a quadratic fit, at the borders too —
    /// which is the whole difference from a central difference, whose first and last
    /// points are one-sided and wrong.
    #[test]
    fn quadratic_is_exact_everywhere_including_the_borders() {
        let y: Vec<f64> = (0..9).map(|i| (i * i) as f64).collect();
        let d = deriv(&y).expect("9 points");
        for (i, v) in d.iter().enumerate() {
            assert!(
                (v - 2.0 * i as f64).abs() < 1e-9,
                "d[{i}] = {v}, want {}",
                2 * i
            );
        }
    }

    /// C `calcUtil.c:73` — `deriv` IS `nderiv` with npts=2. Compiled C agrees on
    /// AA=[0,1,10,3,4,5,6,7,20].
    #[test]
    fn deriv_is_nderiv_with_two_points_per_side() {
        let y = [0.0, 1.0, 10.0, 3.0, 4.0, 5.0, 6.0, 7.0, 20.0];
        assert_eq!(deriv(&y), nderiv(&y, 2));
    }

    /// npts is points PER SIDE, so it needs `2*npts+1` points to fit: npts=0 gives
    /// m=1 and `fitpoly` rejects it (compiled C: `NDERIV(AA,0)` is status -1).
    #[test]
    fn npts_below_one_has_no_fit() {
        let y = [0.0, 1.0, 10.0, 3.0, 4.0, 5.0, 6.0, 7.0, 20.0];
        assert_eq!(nderiv(&y, 0), None);
        assert_eq!(nderiv(&y, -1), None);
        // ... and a window too short for any 3-point fit has none either.
        assert_eq!(deriv(&[1.0, 2.0]), None);
    }

    /// The clamp: `npts` cannot exceed half the window (`aCalcPerform.c:601`).
    /// Compiled C: `NDERIV(AA,100)` and `NDERIV(AA,4)` are the same nine numbers.
    #[test]
    fn npts_is_clamped_to_half_the_window() {
        let y = [0.0, 1.0, 10.0, 3.0, 4.0, 5.0, 6.0, 7.0, 20.0];
        assert_eq!(nderiv(&y, 100), nderiv(&y, 4));
    }
}
